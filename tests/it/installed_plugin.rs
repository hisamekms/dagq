//! `up` and `plan` without `--plugin-dir` load the claude-dagq plugin the
//! user installed, so they make sure `claude plugin list --json` shows it
//! enabled before opening anything, and say how to install it otherwise
//! (ADR-t617-2 decision 4). With `--plugin-dir` they do not ask.

use crate::common;

use common::lifecycle::*;

use dagq::{
    application::PluginState, infrastructure::adapters::plugin_state,
    infrastructure::sqlite::SqliteQueue, lifecycle,
};
use std::{fs, path::PathBuf};

const ENABLED: &str = r#"[{"id":"claude-dagq@dagq","version":"0.4.0","scope":"user","enabled":true,"installPath":"/x"}]"#;

/// Have the fixture's `claude` fail `plugin list`.
fn remove_listing(fixture: &Fixture) {
    let mut path = fixture.options.claude.clone().into_os_string();
    path.push(".plugins");
    let _ = fs::remove_file(PathBuf::from(path));
}

/// What the fixture's `claude` was asked for its plugins and where, or
/// `None` when it was not asked.
fn plugin_call(fixture: &Fixture) -> Option<String> {
    let mut path = fixture.options.claude.clone().into_os_string();
    path.push(".plugin-args");
    fs::read_to_string(PathBuf::from(path)).ok()
}

/// The listings that are not an enabled claude-dagq, with what the error
/// says of each; `None` writes no listing, so the command fails.
fn refusals() -> Vec<(Option<&'static str>, &'static str)> {
    vec![
        (Some("[]"), "the claude-dagq plugin is not installed"),
        (
            Some(r#"[{"id":"other@market","enabled":true}]"#),
            "the claude-dagq plugin is not installed",
        ),
        (
            Some(r#"[{"id":"claude-dagq@dagq","enabled":false}]"#),
            "the claude-dagq plugin is installed but disabled",
        ),
        (
            Some("No plugins installed."),
            "whether the claude-dagq plugin is installed could not be checked",
        ),
        (
            None,
            "whether the claude-dagq plugin is installed could not be checked",
        ),
    ]
}

fn assert_install_hint(message: &str, command: &str) {
    assert!(
        message.contains(
            "Install it with `claude plugin marketplace add hisamekms/dagq` and \
`claude plugin install claude-dagq@dagq`"
        ),
        "{message}"
    );
    assert!(
        message.contains(&format!("then run `dagq {command}` again")),
        "{message}"
    );
    assert!(message.contains("pass --plugin-dir"), "{message}");
}

#[test]
fn plugin_state_reads_the_json_plugin_list() {
    assert_eq!(
        plugin_state(ENABLED, "claude-dagq").unwrap(),
        PluginState::Enabled
    );
    assert_eq!(
        plugin_state("[]", "claude-dagq").unwrap(),
        PluginState::Missing
    );
    // Enabled in any scope or marketplace counts.
    assert_eq!(
        plugin_state(
            r#"[{"id":"claude-dagq@dagq","enabled":false},{"id":"claude-dagq@fork","enabled":true}]"#,
            "claude-dagq"
        )
        .unwrap(),
        PluginState::Enabled
    );
    assert_eq!(
        plugin_state(
            r#"[{"id":"claude-dagq@dagq","enabled":false},{"id":"claude-dagq-extra@dagq","enabled":true}]"#,
            "claude-dagq"
        )
        .unwrap(),
        PluginState::Disabled(vec!["claude-dagq@dagq".into()])
    );
    for unreadable in [
        "No plugins installed.",
        "{}",
        r#"[{"enabled":true}]"#,
        r#"[{"id":"claude-dagq@dagq"}]"#,
    ] {
        assert!(
            plugin_state(unreadable, "claude-dagq").is_err(),
            "{unreadable}"
        );
    }
}

/// Without `--plugin-dir`, an enabled claude-dagq lets `up` start as
/// before, the listing asked for in the repository; anything else stops it
/// before a supervisor or a workspace, with the install commands.
#[test]
fn up_without_a_plugin_dir_needs_the_installed_plugin() {
    let mut fixture = fixture();
    fixture.options.plugin_dir = None;
    for (listed, reason) in refusals() {
        match listed {
            Some(listed) => list_plugins(&fixture, listed),
            None => remove_listing(&fixture),
        }
        let cmux = FakeCmux::default();
        let launchd = FakeLaunchd::new(&fixture.location.db);
        let processes = FakeProcesses::default();
        let message = format!(
            "{:#}",
            try_up(&fixture, &cmux, &launchd, &processes).unwrap_err()
        );
        assert!(message.starts_with(reason), "{listed:?}: {message}");
        assert!(
            message.contains("the inbox session would start"),
            "{message}"
        );
        assert_install_hint(&message, "up");
        assert!(
            message.ends_with("; the supervisor was not started"),
            "{message}"
        );
        if listed.is_some_and(|listed| listed.contains("false")) {
            assert!(
                message.contains("`claude plugin enable claude-dagq@dagq`"),
                "{message}"
            );
        }
        assert!(launchd.installs.lock().unwrap().is_empty());
        assert!(cmux.workspaces.lock().unwrap().is_empty());
        assert_eq!(cmux.detached_preflights.lock().unwrap().len(), 0);
        let queue = SqliteQueue::open(&fixture.location.db).unwrap();
        assert!(queue.supervisors().unwrap().is_empty());
    }

    list_plugins(&fixture, ENABLED);
    let cmux = FakeCmux::default();
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();
    let report = up(&fixture, &cmux, &launchd, &processes);
    assert_eq!(report["supervisor"]["outcome"], "started", "{report}");
    assert_eq!(report["inbox"]["outcome"], "created", "{report}");
    let call = plugin_call(&fixture).unwrap();
    let root = fixture.repo.canonicalize().unwrap();
    assert_eq!(
        call,
        format!("plugin list --json\n{}\n", root.display()),
        "{call}"
    );
    let workspaces = cmux.workspaces.lock().unwrap();
    let inbox = workspaces
        .iter()
        .find(|workspace| workspace.0.ends_with("inbox"))
        .unwrap();
    assert!(!inbox.3.contains("--plugin-dir"), "{}", inbox.3);
}

/// With `--plugin-dir`, `up` does not ask for the installed plugins.
#[test]
fn up_with_a_plugin_dir_does_not_check_the_installed_plugin() {
    let fixture = fixture();
    assert!(fixture.options.plugin_dir.is_some());
    let cmux = FakeCmux::default();
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();
    let report = up(&fixture, &cmux, &launchd, &processes);
    assert_eq!(report["supervisor"]["outcome"], "started", "{report}");
    assert_eq!(plugin_call(&fixture), None);
}

/// `plan` without `--plugin-dir` opens a planner only when claude-dagq is
/// enabled, and with it does not ask.
#[test]
fn plan_without_a_plugin_dir_needs_the_installed_plugin() {
    let fixture = fixture();
    let runner = fixture._dir.path().join("dagq-binary");
    fs::write(&runner, "#!/bin/sh\n").unwrap();
    let options = lifecycle::PlanOptions {
        claude: fixture.options.claude.clone(),
        plugin_dir: None,
        runner,
        user_config: None,
    };
    let queue = SqliteQueue::open(&fixture.location.db).unwrap();
    for (listed, reason) in refusals() {
        match listed {
            Some(listed) => list_plugins(&fixture, listed),
            None => remove_listing(&fixture),
        }
        let cmux = FakeCmux::default();
        let message = format!(
            "{:#}",
            lifecycle::plan(&fixture.location, &fixture.repo, &cmux, &options).unwrap_err()
        );
        assert!(message.starts_with(reason), "{listed:?}: {message}");
        assert!(
            message.contains("the planner session would start"),
            "{message}"
        );
        assert_install_hint(&message, "plan");
        assert!(message.ends_with("; no planner was opened"), "{message}");
        assert!(cmux.workspaces.lock().unwrap().is_empty());
        assert!(queue.planners(true).unwrap().is_empty());
    }

    list_plugins(&fixture, ENABLED);
    let cmux = FakeCmux::default();
    let opened = lifecycle::plan(&fixture.location, &fixture.repo, &cmux, &options).unwrap();
    assert_eq!(opened["planner"]["id"], 1, "{opened}");
    assert!(
        plugin_call(&fixture)
            .unwrap()
            .starts_with("plugin list --json\n")
    );
    assert!(
        !cmux.workspaces.lock().unwrap()[0]
            .3
            .contains("--plugin-dir")
    );

    // With --plugin-dir, nothing is asked, even with nothing installed.
    let mut stub = fixture.options.claude.clone().into_os_string();
    stub.push(".plugin-args");
    fs::remove_file(PathBuf::from(stub)).unwrap();
    list_plugins(&fixture, "[]");
    let with_dir = lifecycle::PlanOptions {
        plugin_dir: fixture.options.plugin_dir.clone(),
        ..options
    };
    lifecycle::plan(&fixture.location, &fixture.repo, &cmux, &with_dir).unwrap();
    assert_eq!(plugin_call(&fixture), None);
}

/// The `up` the runtime runs to start a supervisor again (after an
/// `install --allow-breaking` drain or an automatic update) does not check
/// the installed plugin, as a handoff does not: `LocalBinaries` runs it
/// with `DAGQ_UP_RESTART`, which `up` reads as a restart.
#[test]
fn a_restart_by_the_runtime_does_not_check_the_installed_plugin() {
    use dagq::application::install::Binaries;
    let mut fixture = fixture();
    fixture.options.plugin_dir = None;
    fixture.environment.restart = true;
    let cmux = FakeCmux::default();
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();
    let report = up(&fixture, &cmux, &launchd, &processes);
    assert_eq!(report["supervisor"]["outcome"], "started", "{report}");
    assert_eq!(plugin_call(&fixture), None);

    let binary = fixture._dir.path().join("restarted-dagq");
    fs::write(
        &binary,
        "#!/bin/sh\nprintf '{\"restart\": \"%s\", \"arguments\": \"%s\"}' \"$DAGQ_UP_RESTART\" \"$*\"\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
    let printed = dagq::infrastructure::binaries::LocalBinaries
        .run(&binary, &["up".into(), "--in-cmux".into()])
        .unwrap();
    assert_eq!(lifecycle::UP_RESTART_ENV, "DAGQ_UP_RESTART");
    assert_eq!(
        printed,
        serde_json::json!({"restart": "1", "arguments": "up --in-cmux"})
    );
}
