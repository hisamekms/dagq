//! `up` without `--plugin-dir` relies on the claude-dagq plugin the user
//! installed, so it makes sure `claude plugin list --json` shows it enabled
//! before starting anything, and says how to install it otherwise
//! (ADR-t617-2 decision 4); `dagq inbox` checks the same
//! (`lifecycle_inbox`). With `--plugin-dir` it does not ask. `plan`
//! opens nothing any more (ADR-t1394-1), so it asks nothing either.

use crate::common;

use common::lifecycle::*;

use dagq::{application::planner::PLAN_REFUSED, infrastructure::sqlite::SqliteQueue, lifecycle};
use serde_json::Value;
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
        let launchd = FakeLaunchd::new(&fixture.location.db);
        let processes = FakeProcesses::default();
        let message = format!("{:#}", try_up(&fixture, &launchd, &processes).unwrap_err());
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
        let queue = SqliteQueue::open(&fixture.location.db).unwrap();
        assert!(queue.supervisors().unwrap().is_empty());
    }

    list_plugins(&fixture, ENABLED);
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();
    let report = up(&fixture, &launchd, &processes);
    assert_eq!(report["supervisor"]["outcome"], "started", "{report}");
    assert_eq!(report["inbox"]["outcome"], "not_opened", "{report}");
    let call = plugin_call(&fixture).unwrap();
    let root = fixture.repo.canonicalize().unwrap();
    assert_eq!(
        call,
        format!("plugin list --json\n{}\n", root.display()),
        "{call}"
    );
}

/// With `--plugin-dir`, `up` does not ask for the installed plugins.
#[test]
fn up_with_a_plugin_dir_does_not_check_the_installed_plugin() {
    let fixture = fixture();
    assert!(fixture.options.plugin_dir.is_some());
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();
    let report = up(&fixture, &launchd, &processes);
    assert_eq!(report["supervisor"]["outcome"], "started", "{report}");
    assert_eq!(plugin_call(&fixture), None);
}

/// `plan` opens no planner any more (ADR-t1394-1): without `--plugin-dir`,
/// whatever the listing says, it asks nothing of the installed plugin and
/// gives no install hint, only the guidance to ask the inbox, and no
/// planner is recorded.
#[test]
fn plan_refuses_without_asking_for_the_installed_plugin() {
    let fixture = fixture();
    let queue = SqliteQueue::open(&fixture.location.db).unwrap();
    let claude = fixture.options.claude.to_str().unwrap();
    for (listed, _) in refusals() {
        match listed {
            Some(listed) => list_plugins(&fixture, listed),
            None => remove_listing(&fixture),
        }
        let output = crate::common::cli::invoke_as(
            None,
            &fixture.location.db,
            &["plan", "--claude", claude],
        );
        assert!(!output.status.success(), "{listed:?}: {output:?}");
        let error: Value = serde_json::from_slice(&output.stderr).unwrap();
        let message = error["error"].as_str().unwrap();
        assert_eq!(message, PLAN_REFUSED, "{listed:?}");
        assert!(!message.contains("claude plugin install"), "{message}");
        assert_eq!(plugin_call(&fixture), None);
        assert!(queue.planners(true).unwrap().is_empty());
    }
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
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();
    let report = up(&fixture, &launchd, &processes);
    assert_eq!(report["supervisor"]["outcome"], "started", "{report}");
    assert_eq!(plugin_call(&fixture), None);

    let binary = fixture._dir.path().join("restarted-dagq");
    crate::common::template::script(
        &binary,
        "#!/bin/sh\nprintf '{\"restart\": \"%s\", \"arguments\": \"%s\"}' \"$DAGQ_UP_RESTART\" \"$*\"\n",
    );

    let printed = dagq::infrastructure::binaries::LocalBinaries
        .run(&binary, &["up".into(), "--auto-update".into()])
        .unwrap();
    assert_eq!(lifecycle::UP_RESTART_ENV, "DAGQ_UP_RESTART");
    assert_eq!(
        printed,
        serde_json::json!({"restart": "1", "arguments": "up --auto-update"})
    );
}

/// The release update reaches the installed plugin through the `claude` it
/// is given (ADR-t618-2): the version from `plugin list --json`, and the
/// update as the marketplace's and then the plugin's, in the directory
/// given, stopping at the first failure with what it printed.
#[test]
fn the_installed_plugin_is_read_and_updated_through_claude() {
    use dagq::application::InstalledPlugin;
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("calls");
    let claude = dir.path().join("claude");
    crate::common::template::script(
        &claude,
        format!(
            "#!/bin/sh\nprintf '%s|%s\\n' \"$PWD\" \"$*\" >> \"${{0%/*}}/calls\"\ncase \"$*\" in\n\
'plugin list --json') printf '%s' '{ENABLED}' ;;\n\
'plugin update claude-dagq@dagq') [ -f \"${{0%/*}}/fail\" ] && {{ echo 'fetch failed' >&2; exit 1; }}; echo updated ;;\n\
*) echo ok ;;\nesac\n",
        ),
    );

    let cwd = dir.path().canonicalize().unwrap();
    let plugin = dagq::infrastructure::adapters::ClaudePlugin {
        executable: claude.clone(),
        cwd: cwd.clone(),
    };
    assert_eq!(plugin.version().unwrap().as_deref(), Some("0.4.0"));
    let runs = plugin.update();
    assert_eq!(
        runs.iter().map(|r| r.command.as_str()).collect::<Vec<_>>(),
        [
            "claude plugin marketplace update dagq",
            "claude plugin update claude-dagq@dagq"
        ]
    );
    assert!(runs.iter().all(|r| r.succeeded), "{runs:?}");
    assert_eq!(runs[1].output, "updated");
    let calls = fs::read_to_string(&log).unwrap();
    assert_eq!(
        calls,
        format!(
            "{cwd}|plugin list --json\n{cwd}|plugin marketplace update dagq\n{cwd}|plugin update claude-dagq@dagq\n",
            cwd = cwd.display()
        )
    );

    fs::write(dir.path().join("fail"), "").unwrap();
    let runs = plugin.update();
    assert_eq!(runs.len(), 2);
    assert!(runs[0].succeeded && !runs[1].succeeded, "{runs:?}");
    assert!(runs[1].output.contains("fetch failed"), "{runs:?}");

    // A claude that is not there: the first command fails, the second is
    // not run.
    let missing = dagq::infrastructure::adapters::ClaudePlugin {
        executable: dir.path().join("no-claude"),
        cwd,
    };
    let runs = missing.update();
    assert_eq!(runs.len(), 1);
    assert!(!runs[0].succeeded);
    assert!(missing.version().is_err());
}
