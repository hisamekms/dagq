//! `dagq inbox` (ADR-t2159-1 decision 2): the inbox's agent started in the
//! terminal of the person who types it, in place of the `dagq` process,
//! against a stub `claude` that records what it was started with; its
//! record (`inbox_opened`) and the guardrail `status` and `doctor` judge by
//! it; and the refusals that open nothing: inside an inbox, and without the
//! installed plugin.

use crate::common;

use common::{Bounded, WithoutActor, lifecycle::*, service::OwnedByTest};

use dagq::{
    domain::SessionRole,
    infrastructure::sqlite::SqliteQueue,
    lifecycle::{self, INBOX_INSIDE_REFUSED, InboxOptions},
};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

const ENABLED: &str = r#"[{"id":"claude-dagq@dagq","version":"0.4.0","scope":"user","enabled":true,"installPath":"/x"}]"#;

/// A `claude` in the fixture's directory that answers `--version` and
/// `plugin list` (from `<stub>.plugins`), and as the inbox's agent writes
/// its arguments, the inbox's variables and its directory next to itself.
fn stub_claude(fixture: &Fixture) -> PathBuf {
    let stub = fixture._dir.path().join("claude-inbox");
    common::template::script(
        &stub,
        "#!/bin/sh\n\
case \"$1\" in --version) printf 'claude-stub 0.0.0\\n'; exit 0 ;; plugin) exec cat \"$0.plugins\" ;; esac\n\
printf '%s\\n' \"$@\" > \"$0.argv\"\n\
printf 'DAGQ_ROLE=%s\\nDAGQ_QUEUE=%s\\nDAGQ_ACTOR_ID=%s\\nDAGQ_SESSION_KIND=%s\\n' \
\"$DAGQ_ROLE\" \"$DAGQ_QUEUE\" \"$DAGQ_ACTOR_ID\" \"$DAGQ_SESSION_KIND\" > \"$0.env\"\n\
pwd > \"$0.cwd\"\n",
    );
    stub
}

/// What the stub wrote as `suffix` (`argv`, `env`, `cwd`), or `None` when
/// it was not started as the agent.
fn started(stub: &Path, suffix: &str) -> Option<String> {
    let mut path = stub.as_os_str().to_owned();
    path.push(format!(".{suffix}"));
    fs::read_to_string(PathBuf::from(path)).ok()
}

/// `dagq --db <queue> inbox --repo <repo> --claude <stub> <args>` from a
/// directory outside the repository, with `env` as the caller's actor.
fn inbox(fixture: &Fixture, stub: &Path, env: &[(&str, &str)], args: &[&str]) -> Output {
    let elsewhere = fixture._dir.path().join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_dagq"));
    command.without_actor_env().owned_by_test();
    command.env_remove("DAGQ_QUEUE");
    command.envs(env.iter().copied());
    command
        .current_dir(&elsewhere)
        .arg("--db")
        .arg(&fixture.location.db)
        .arg("inbox")
        .arg("--repo")
        .arg(&fixture.repo)
        .arg("--claude")
        .arg(stub)
        .args(args)
        .bounded_output()
        .unwrap()
}

fn error_of(output: &Output) -> String {
    assert!(!output.status.success(), "{output:?}");
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    error["error"].as_str().unwrap().to_owned()
}

fn opened(fixture: &Fixture) -> Option<Value> {
    SqliteQueue::open(&fixture.location.db)
        .unwrap()
        .latest_event_of("inbox_opened")
        .unwrap()
        .map(|event| event.payload)
}

/// `dagq inbox` starts the inbox's `claude` in its own place: with the
/// inbox's settings (`permissions.deny` only, under the queue's directory),
/// the plugin directory and the inbox's prompt, `DAGQ_ROLE=inbox` and the
/// queue in its environment, in the repository's main checkout. It records
/// `inbox_opened` first, without a workspace, and `status` and `doctor`
/// judge the inbox's guardrail by it: none before any open, `true` after.
#[test]
fn dagq_inbox_starts_the_inbox_agent_in_this_terminal_and_records_its_open() {
    let fixture = fixture();
    let stub = stub_claude(&fixture);
    let db = fixture.location.db.canonicalize().unwrap();
    let status = || dagq::compose::status_for(&db, Some(SessionRole::Inbox)).unwrap();
    assert_eq!(
        status()["inbox_guardrail"],
        json!({"guardrail": null, "reason": "no_record"})
    );

    let plugin_dir = fixture.options.plugin_dir.clone().unwrap();
    let output = inbox(
        &fixture,
        &stub,
        &[],
        &["--plugin-dir", plugin_dir.to_str().unwrap()],
    );
    assert!(output.status.success(), "{output:?}");

    let settings = db.with_file_name("claude-inbox-settings.json");
    let argv = started(&stub, "argv").expect("the stub was started as the agent");
    let argv: Vec<&str> = argv.lines().collect();
    assert_eq!(
        argv[..5],
        [
            "--settings",
            settings.to_str().unwrap(),
            "--plugin-dir",
            plugin_dir.canonicalize().unwrap().to_str().unwrap(),
            "--",
        ],
        "{argv:?}"
    );
    assert!(
        argv[5].starts_with(&format!(
            "You are the inbox of the dagq queue at {}",
            db.display()
        )),
        "{argv:?}"
    );
    assert_eq!(
        started(&stub, "env").unwrap(),
        format!(
            "DAGQ_ROLE=inbox\nDAGQ_QUEUE={}\nDAGQ_ACTOR_ID=inbox\nDAGQ_SESSION_KIND=inbox\n",
            db.display()
        )
    );
    assert_eq!(
        started(&stub, "cwd").unwrap().trim_end(),
        fixture.repo.canonicalize().unwrap().to_str().unwrap()
    );

    // Only `permissions.deny`: no hook, no idle marker, no suggestion
    // setting, no autoMode.
    let written: Value = serde_json::from_slice(&fs::read(&settings).unwrap()).unwrap();
    assert_eq!(
        written.as_object().unwrap().keys().collect::<Vec<_>>(),
        ["permissions"]
    );
    let deny: Vec<&str> = written["permissions"]["deny"]
        .as_array()
        .unwrap()
        .iter()
        .map(|rule| rule.as_str().unwrap())
        .collect();
    for rule in [
        "Bash(DAGQ_ROLE=*)",
        "Bash(unset DAGQ_ACTOR_ID*)",
        // The inbox's role is refused what its policy does not grant.
        "Bash(dagq request decline:*)",
    ] {
        assert!(deny.contains(&rule), "{rule}: {deny:?}");
    }
    assert!(!deny.contains(&"Bash(dagq answer:*)"), "{deny:?}");
    assert!(!deny.contains(&"Bash(dagq inbox:*)"), "{deny:?}");

    assert_eq!(
        opened(&fixture).unwrap(),
        json!({"guardrail": true, "settings": settings, "provider": "claude"})
    );
    assert_eq!(
        status()["inbox_guardrail"],
        json!({"guardrail": true, "settings": settings, "provider": "claude"})
    );
    let doctor = dagq::compose::doctor(&db, false).unwrap();
    assert_eq!(doctor["inbox_guardrail"]["guardrail"], true, "{doctor}");
}

/// Inside an inbox (`DAGQ_ROLE=inbox`) `dagq inbox` opens nothing and
/// records nothing, whether `DAGQ_QUEUE` names this queue, another one or
/// none (ADR-t2159-1 decision 2).
#[test]
fn dagq_inbox_inside_an_inbox_opens_nothing_whatever_the_queue() {
    let fixture = fixture();
    let stub = stub_claude(&fixture);
    let plugin_dir = fixture.options.plugin_dir.clone().unwrap();
    let this = fixture.location.db.canonicalize().unwrap();
    let other = fixture._dir.path().join("other").join("queue.db");
    for queue in [Some(this.as_path()), Some(other.as_path()), None] {
        let mut env = vec![("DAGQ_ROLE", "inbox")];
        if let Some(queue) = queue {
            env.push(("DAGQ_QUEUE", queue.to_str().unwrap()));
        }
        let output = inbox(
            &fixture,
            &stub,
            &env,
            &["--plugin-dir", plugin_dir.to_str().unwrap()],
        );
        assert_eq!(error_of(&output), INBOX_INSIDE_REFUSED, "{queue:?}");
        assert_eq!(started(&stub, "argv"), None, "{queue:?}");
        assert_eq!(opened(&fixture), None, "{queue:?}");
    }
}

/// Without `--plugin-dir`, `dagq inbox` checks the installed plugin as `up`
/// does: a disabled one stops it with the way to install or enable it,
/// before anything is started or recorded; an enabled one opens it.
#[test]
fn dagq_inbox_without_the_enabled_plugin_stops_with_the_way_to_install_it() {
    let fixture = fixture();
    let stub = stub_claude(&fixture);
    let plugins = |listed: &str| {
        let mut path = stub.as_os_str().to_owned();
        path.push(".plugins");
        fs::write(PathBuf::from(path), listed).unwrap();
    };
    plugins(r#"[{"id":"claude-dagq@dagq","enabled":false}]"#);
    let message = error_of(&inbox(&fixture, &stub, &[], &[]));
    assert!(
        message.starts_with("the claude-dagq plugin is installed but disabled"),
        "{message}"
    );
    assert!(
        message.contains("`claude plugin install claude-dagq@dagq`")
            && message.contains("`claude plugin enable claude-dagq@dagq`")
            && message.contains("then run `dagq inbox` again")
            && message.ends_with("; the inbox was not opened"),
        "{message}"
    );
    assert_eq!(started(&stub, "argv"), None);
    assert_eq!(opened(&fixture), None);

    plugins(ENABLED);
    let output = inbox(&fixture, &stub, &[], &[]);
    assert!(output.status.success(), "{output:?}");
    let argv = started(&stub, "argv").unwrap();
    assert!(!argv.contains("--plugin-dir"), "{argv}");
    assert_eq!(opened(&fixture).unwrap()["guardrail"], true);
}

/// The inbox's prompt carries the language of the repository's `dagq.toml`
/// over the user's `config.toml` (ADR-t616-2), and a mistake in it stops
/// `dagq inbox` before anything is recorded.
#[test]
fn the_inbox_prompt_carries_the_language_and_a_wrong_one_opens_nothing() {
    let mut fixture = fixture();
    let config = fixture._dir.path().join("config.toml");
    fs::write(&config, "[language]\ntag = 'ja_JP'\n").unwrap();
    fixture.environment.user_config = Some(config.clone());
    let options = InboxOptions {
        plugin_dir: fixture.options.plugin_dir.clone(),
        agent: fixture.options.claude.clone(),
    };
    let error = lifecycle::inbox(
        &fixture.location,
        &fixture.repo,
        &fixture.environment,
        &options,
    )
    .unwrap_err();
    let error = format!("{error:#}");
    assert!(
        error.contains("BCP 47") && error.ends_with("the inbox was not opened"),
        "{error}"
    );
    assert_eq!(opened(&fixture), None);

    fs::write(&config, "[language]\ntag = 'ja'\n").unwrap();
    let command = lifecycle::inbox(
        &fixture.location,
        &fixture.repo,
        &fixture.environment,
        &options,
    )
    .unwrap();
    let prompt = command.get_args().last().unwrap().to_string_lossy();
    assert!(prompt.contains("You are the inbox of"), "{prompt}");
    assert!(prompt.contains("BCP 47 tag `ja`"), "{prompt}");
    assert_eq!(opened(&fixture).unwrap()["guardrail"], true);
}
