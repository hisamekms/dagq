//! `dagq install --allow-breaking`'s drain against fake binaries, launchd
//! and process signals: a breaking migration is drained only when allowed,
//! the drained supervisor's settings and provider links carry over to the
//! `up` that starts it again, and the drain (`down --wait`) unloads the
//! queue's agents of another label and waits for their supervisors.

use crate::common;
use dagq::domain::LeaseToken;

use common::install::*;
use common::lifecycle::*;

use anyhow::Result;
use dagq::{
    domain::{
        SupervisorMode,
        slot_limits::{Setting, SettingSource, SlotLimits},
    },
    infrastructure::sqlite::SqliteQueue,
};
use serde_json::{Value, json};
use std::{fs, sync::Mutex, thread, time::Duration};

/// A build with a breaking migration is refused without `--allow-breaking`
/// and replaces nothing; with it, the supervisor is drained, the queue
/// migrated, the binary replaced and `up` run with the drained supervisor's
/// parallelism, automatic update, wait limit and load threshold, without
/// `--in-cmux` though the drained one was registered in that retired mode:
/// it starts under launchd (ADR-t1433-4). A rollback past a breaking
/// migration is refused, and so is one without a previous binary.
#[test]
fn install_drains_only_for_a_breaking_migration_when_allowed() {
    use dagq::application::install::Source;
    let fixture = fixture();
    let queue = handoff_supervisor(&fixture, "live", SupervisorMode::InCmux);
    let processes = FakeProcesses::default();
    let binaries = FakeBinaries::new(&[(27, true), (28, false)], false);
    let drained = Mutex::new(0);
    let down = || -> Result<Value> {
        *drained.lock().unwrap() += 1;
        Ok(json!({"outcome": "stopped"}))
    };
    let error = format!(
        "{:#}",
        install_with(
            &fixture,
            &binaries,
            &processes,
            &down,
            &install_options(Source::Binary("/built/dagq".into())),
        )
        .unwrap_err()
    );
    assert!(error.contains("breaking migration(s) 28"), "{error}");
    assert!(error.contains("--allow-breaking"), "{error}");
    assert_eq!(binaries.calls(), ["probe /built/dagq"]);
    assert_eq!(*drained.lock().unwrap(), 0);

    let mut options = install_options(Source::Binary("/built/dagq".into()));
    options.allow_breaking = true;
    let report = install_with(&fixture, &binaries, &processes, &down, &options).unwrap();
    assert_eq!(*drained.lock().unwrap(), 1);
    assert_eq!(report["drained"]["outcome"], "stopped", "{report}");
    assert_eq!(report["up"]["supervisor"]["outcome"], "started");
    assert_eq!(
        binaries.calls()[2..],
        [
            "migrate /built/dagq",
            "replace /built/dagq /opt/bin/dagq",
            "run /opt/bin/dagq",
        ]
    );
    let db = fixture.location.db.to_str().unwrap();
    assert_eq!(
        binaries.up.lock().unwrap()[0],
        [
            "--db",
            db,
            "up",
            "--parallel",
            "4",
            "--claude",
            "/opt/claude"
        ]
    );

    // A drained supervisor with the automatic update, a wait limit and a
    // limit of the runtime's planners gets them back from the `up`, once
    // each even when the restart names them (task 941).
    queue
        .set_auto_update(&LeaseToken::new("live"), true)
        .unwrap();
    queue
        .set_slot_limits(
            &LeaseToken::new("live"),
            slot_limits(SettingSource::Flag, 2),
        )
        .unwrap();
    let binaries = FakeBinaries::new(&[(28, false)], false);
    install_with(&fixture, &binaries, &processes, &down, &options).unwrap();
    options.restart = vec![
        "--auto-update".into(),
        "--max-waiting".into(),
        "6".into(),
        "--runtime-planners".into(),
        "2".into(),
    ];
    install_with(&fixture, &binaries, &processes, &down, &options).unwrap();
    assert_eq!(
        *binaries.up.lock().unwrap(),
        [
            vec![
                "--db",
                db,
                "up",
                "--parallel",
                "4",
                "--auto-update",
                "--max-waiting",
                "2",
                "--runtime-planners",
                "3",
                "--claude",
                "/opt/claude"
            ],
            vec![
                "--db",
                db,
                "up",
                "--parallel",
                "4",
                "--auto-update",
                "--max-waiting",
                "6",
                "--runtime-planners",
                "2"
            ]
        ]
    );

    // Values the drained supervisor took from `dagq.toml` (or the
    // default) are not baked into the `up`: the started one reads them
    // again (task 698).
    for source in [SettingSource::File, SettingSource::Default] {
        queue
            .set_slot_limits(&LeaseToken::new("live"), slot_limits(source, 2))
            .unwrap();
        let binaries = FakeBinaries::new(&[(28, false)], false);
        options.restart = vec![];
        install_with(&fixture, &binaries, &processes, &down, &options).unwrap();
        assert_eq!(
            binaries.up.lock().unwrap()[0],
            ["--db", db, "up", "--auto-update"],
            "{source:?}"
        );
    }

    // `--max-load 0` (the hold off) recorded on the registration comes
    // back as the same flag; the other cases are `install`'s unit test.
    queue
        .set_max_load(&LeaseToken::new("live"), None, Some(SettingSource::Flag))
        .unwrap();
    let binaries = FakeBinaries::new(&[(28, false)], false);
    install_with(&fixture, &binaries, &processes, &down, &options).unwrap();
    assert_eq!(
        binaries.up.lock().unwrap()[0],
        ["--db", db, "up", "--auto-update", "--max-load", "0"]
    );

    let binaries = FakeBinaries::new(&[], false);
    let dir = tempfile::tempdir().unwrap();
    let mut options = install_options(Source::Rollback);
    options.target = dir.path().join("dagq");
    let error = format!(
        "{:#}",
        install_with(&fixture, &binaries, &processes, &down, &options).unwrap_err()
    );
    assert!(error.contains("no previous binary"), "{error}");
    fs::write(dir.path().join("dagq.previous"), "").unwrap();
    let error = format!(
        "{:#}",
        install_with(&fixture, &binaries, &processes, &down, &options).unwrap_err()
    );
    assert!(error.contains("the queue refuses 0.0.1"), "{error}");
    assert_eq!(
        dagq::application::install::parse_version("dagq 1.2.3\n").unwrap(),
        "1.2.3"
    );
    assert!(dagq::application::install::parse_version("").is_err());
}

/// `parallel` 4, `max_waiting` `max_waiting`, `runtime_planners` 3 and
/// `claim_spacing` 180, all from `source`.
fn slot_limits(source: SettingSource, max_waiting: usize) -> SlotLimits {
    SlotLimits {
        parallel: Setting { value: 4, source },
        max_waiting: Setting {
            value: max_waiting,
            source,
        },
        runtime_planners: Setting { value: 3, source },
        claim_spacing: Setting { value: 180, source },
        max_spikes: dagq::domain::slot_limits::OptionalSetting::from_file(None),
        class_aging: Setting {
            value: 3600,
            source: SettingSource::Default,
        },
        max_investigations: dagq::domain::slot_limits::OptionalSetting::from_file(None),
    }
}

#[test]
fn no_claude_policy_survives_install_drain_and_restart_arguments() {
    use dagq::application::install::Source;
    let fixture = fixture();
    let queue = handoff_supervisor(&fixture, "live", SupervisorMode::InCmux);
    queue
        .set_supervisor_providers(
            &LeaseToken::new("live"),
            &[dagq::domain::worker::ProviderCheck {
                provider: dagq::domain::Provider::Claude,
                executable: "/missing-claude".into(),
                found: false,
                error: Some("provider_disabled".into()),
                modes: vec![],
            }],
        )
        .unwrap();
    let registrations = queue.supervisors().unwrap();
    assert!(
        registrations[0]
            .flag_arguments()
            .contains(&"--no-claude".to_owned())
    );
    let processes = FakeProcesses::default();
    let down = || Ok(json!({"outcome": "stopped"}));
    for explicit in [false, true] {
        let binaries = FakeBinaries::new(&[(28, false)], false);
        let mut options = install_options(Source::Binary("/built/dagq".into()));
        options.allow_breaking = true;
        if explicit {
            options.restart.push("--no-claude".into());
        }
        install_with(&fixture, &binaries, &processes, &down, &options).unwrap();
        assert_eq!(
            binaries.up.lock().unwrap()[0]
                .iter()
                .filter(|a| *a == "--no-claude")
                .count(),
            1
        );
    }
}

/// ADR-t2079-1: the `up` with which `install` starts a drained supervisor
/// again is given Claude Code and Codex as the links `install` was given
/// (`compose::install_restart_arguments`, as the command makes them),
/// never the versions they point at, which an update removes.
#[test]
fn install_starts_a_drained_supervisor_again_with_the_provider_links() {
    use dagq::application::install::Source;
    let fixture = fixture();
    let _queue = handoff_supervisor(&fixture, "live", SupervisorMode::InCmux);
    let processes = FakeProcesses::default();
    let down = || -> Result<Value> { Ok(json!({"outcome": "stopped"})) };
    let claude_version = fixture._dir.path().join("share/claude/versions/2.2.0");
    let codex_version = fixture._dir.path().join("codex/releases/0.2.0/bin/codex");
    for version in [&claude_version, &codex_version] {
        fs::create_dir_all(version.parent().unwrap()).unwrap();
        common::template::script(version, "#!/bin/sh\nprintf '0.2.0\\n'\n");
    }
    let bin = fixture._dir.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink(&claude_version, bin.join("claude")).unwrap();
    std::os::unix::fs::symlink(&codex_version, bin.join("codex")).unwrap();
    let mut options = install_options(Source::Binary("/built/dagq".into()));
    options.allow_breaking = true;
    options.restart = dagq::compose::install_restart_arguments(
        Some(&bin.join("claude")),
        Some(&bin.join("codex")),
        None,
    )
    .unwrap();
    let binaries = FakeBinaries::new(&[(27, true), (28, false)], false);
    install_with(&fixture, &binaries, &processes, &down, &options).unwrap();
    let up = binaries.up.lock().unwrap()[0].clone();
    let after = |flag: &str| {
        let at = up.iter().position(|arg| arg == flag).unwrap();
        up[at + 1].clone()
    };
    assert_eq!(
        after("--claude"),
        bin.join("claude").to_str().unwrap(),
        "{up:?}"
    );
    assert_eq!(
        after("--codex"),
        bin.join("codex").to_str().unwrap(),
        "{up:?}"
    );
    assert!(
        up.iter()
            .all(|arg| !arg.contains("versions/") && !arg.contains("releases/")),
        "{up:?}"
    );
    // No `--cmux` (ADR-t2159-1 decision 6).
    assert!(!up.iter().any(|arg| arg == "--cmux"), "{up:?}");
}

/// `install --allow-breaking`'s drain is `down --wait`: it unloads the
/// queue's agent under another label too, sends its supervisor no second
/// SIGTERM, waits for it to drain before the migration, and starts the
/// supervisor again with the new binary's `up`.
#[test]
fn install_drain_unloads_a_stray_agent_and_waits_for_its_supervisor() {
    use dagq::application::install::Source;
    let fixture = fixture();
    let mut queue = SqliteQueue::open(&fixture.location.db).unwrap();
    let stray_pid = 4_000_002;
    queue
        .register_supervisor(&LeaseToken::new("stray"), stray_pid, 4, "0.0.1")
        .unwrap();
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let stray = launchd.define_other(
        &fixture,
        "com.dagq.stray",
        &fixture.location.db,
        true,
        Some(stray_pid),
    );
    let processes = FakeProcesses::default();
    let binaries = FakeBinaries::new(&[(28, false)], false);
    let drain = || -> Result<Value> { Ok(down(&fixture, &launchd, &processes, true, false)) };
    let mut options = install_options(Source::Binary("/built/dagq".into()));
    options.allow_breaking = true;
    let (report, calls_at_drain_end) = {
        let _waiting = common::within(
            common::STEP_LIMIT,
            "install's drain to return once the stray agent's supervisor deregistered, and its thread to join",
        );
        thread::scope(|scope| {
            let drained = scope.spawn(|| {
                wait_until(&processes, stray_pid, || {
                    !launchd.uninstalls.lock().unwrap().is_empty()
                });
                thread::sleep(Duration::from_millis(200));
                let calls = binaries.calls();
                SqliteQueue::open(&fixture.location.db)
                    .unwrap()
                    .deregister_supervisor(&LeaseToken::new("stray"))
                    .unwrap();
                calls
            });
            let report = install_with(&fixture, &binaries, &processes, &drain, &options).unwrap();
            (report, drained.join().unwrap())
        })
    };
    assert_eq!(calls_at_drain_end, ["probe /built/dagq"]);
    assert_eq!(report["drained"]["outcome"], "stopped", "{report}");
    assert_eq!(
        report["drained"]["stray_agents"],
        json!([{"label": "com.dagq.stray", "plist": stray, "loaded": true, "pid": stray_pid}])
    );
    assert!(!stray.exists());
    assert!(processes.terminated.lock().unwrap().is_empty());
    assert!(queue.supervisors().unwrap().is_empty());
    assert_eq!(
        binaries.calls()[1..],
        [
            "migrate /built/dagq",
            "replace /built/dagq /opt/bin/dagq",
            "run /opt/bin/dagq",
        ]
    );
}
