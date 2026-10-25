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
    lifecycle::{self, CLAUDE_INSTEAD, CODEX_INBOX_PENDING, INBOX_INSIDE_REFUSED, InboxOptions},
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
pwd > \"$0.cwd\"\n\
printf '%s' \"$DAGQ_SERVICE_CREDENTIAL_FILE\" > \"$0.credential\"\n\
printf '%s' \"$$\" > \"$0.pid\"\n\
TZ=UTC0 LC_ALL=C ps -o lstart= -p $$ | sed 's/^ *//; s/ *$//' | tr -d '\\n' > \"$0.start\"\n",
    );
    stub
}

/// A `codex` in the fixture's directory that answers `--version`, and
/// `login status` as logged in only while `<stub>.login` exists, writing
/// the `CODEX_HOME` it was checked with next to itself; started as the
/// inbox's agent it writes its arguments, as the Claude stub does.
fn stub_codex(fixture: &Fixture) -> PathBuf {
    let stub = fixture._dir.path().join("codex-inbox");
    common::template::script(
        &stub,
        "#!/bin/sh\n\
case \"$1\" in --version) printf 'codex-cli 0.0.0\\n'; exit 0 ;; \
login) printf '%s' \"$CODEX_HOME\" > \"$0.home\"; \
if [ -f \"$0.login\" ]; then printf 'Logged in using ChatGPT\\n'; exit 0; fi; \
printf 'Not logged in\\n'; exit 1 ;; esac\n\
printf '%s\\n' \"$@\" > \"$0.argv\"\n",
    );
    stub
}

fn logged_in(stub: &Path) {
    let mut path = stub.as_os_str().to_owned();
    path.push(".login");
    fs::write(PathBuf::from(path), "").unwrap();
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
    // A zone in the person's shell other than the service's changes
    // nothing of the session its token is bound to.
    let output = inbox(
        &fixture,
        &stub,
        &[("TZ", "Pacific/Kiritimati")],
        &["--plugin-dir", plugin_dir.to_str().unwrap()],
    );
    assert!(output.status.success(), "{output:?}");

    // The agent, in the place of `dagq inbox`, was given the file of a
    // token for the inbox of its own process (ADR-t3102-1 decisions 2 and
    // 3), outside the repository, as the service and the supervisor read
    // the process's start.
    let credential = PathBuf::from(started(&stub, "credential").unwrap());
    assert!(
        credential.starts_with(db.parent().unwrap().join("service")),
        "{credential:?}"
    );
    let token = dagq::infrastructure::queue_service::read_token(&credential).unwrap();
    let principal = dagq::infrastructure::queue_service::lookup(db.parent().unwrap(), &token)
        .unwrap()
        .expect("the token names a principal");
    assert_eq!(
        principal,
        dagq::domain::queue_service::Principal::inbox(
            dagq::domain::queue_service::SessionProcess {
                pid: started(&stub, "pid").unwrap().parse().unwrap(),
                start: started(&stub, "start").unwrap(),
            }
        )
    );

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

/// A Codex inbox, chosen by `--provider codex` or by `[roles.inbox]`, is
/// not opened yet (ADR-t1181-1): before that, a person's Codex settings
/// that Codex would write to stop it with where they are and the lines to
/// add, left unwritten; then it is refused with why. Neither starts
/// Claude in its place or records an open, and a Claude model given to
/// Codex in `[roles.inbox]` is a mistake of the setting.
#[test]
fn a_codex_inbox_is_refused_with_why_and_starts_nothing() {
    let fixture = fixture();
    let stub = stub_claude(&fixture);
    let plugin_dir = fixture.options.plugin_dir.clone().unwrap();
    let plugin_dir = plugin_dir.to_str().unwrap();
    let codex_home = fixture._dir.path().join("codex-home");
    fs::create_dir_all(&codex_home).unwrap();
    let env = [("CODEX_HOME", codex_home.to_str().unwrap())];
    let codex_stub = stub_codex(&fixture);
    logged_in(&codex_stub);
    let codex_stub = codex_stub.to_str().unwrap();
    let codex = [
        "--plugin-dir",
        plugin_dir,
        "--provider",
        "codex",
        "--codex",
        codex_stub,
    ];
    let config = codex_home.join("config.toml");

    let message = error_of(&inbox(&fixture, &stub, &env, &codex));
    assert!(
        message.starts_with("the Codex inbox was not opened: ")
            && message.contains(config.to_str().unwrap())
            && message.contains("[tui]\nscreen_reader_detection_done = true"),
        "{message}"
    );
    assert!(!config.exists());

    fs::write(&config, "[tui]\nscreen_reader_detection_done = true\n").unwrap();
    let pending = format!("{CODEX_INBOX_PENDING}; {CLAUDE_INSTEAD}; the inbox was not opened");
    assert_eq!(error_of(&inbox(&fixture, &stub, &env, &codex)), pending);
    // A relative CODEX_HOME is the one under the main checkout Codex
    // starts in, not under the directory `dagq inbox` was typed in (which
    // has the setting here and is not read).
    let typed_in = fixture._dir.path().join("elsewhere").join("codex-rel");
    fs::create_dir_all(&typed_in).unwrap();
    fs::write(
        typed_in.join("config.toml"),
        "[tui]\nscreen_reader_detection_done = true\n",
    )
    .unwrap();
    let relative = [("CODEX_HOME", "codex-rel")];
    let message = error_of(&inbox(&fixture, &stub, &relative, &codex));
    let at_checkout = fixture.repo.join("codex-rel").join("config.toml");
    assert!(
        message.contains("does not exist") && message.contains(at_checkout.to_str().unwrap()),
        "{message}"
    );
    fs::create_dir_all(at_checkout.parent().unwrap()).unwrap();
    fs::write(&at_checkout, "[tui]\nscreen_reader_detection_done = true\n").unwrap();
    assert_eq!(
        error_of(&inbox(&fixture, &stub, &relative, &codex)),
        pending
    );
    let toml = fixture.repo.join("dagq.toml");
    let before = fs::read_to_string(&toml).unwrap_or_default();
    fs::write(
        &toml,
        format!("{before}\n[roles.inbox]\nprovider = \"codex\"\n"),
    )
    .unwrap();
    let configured = ["--plugin-dir", plugin_dir, "--codex", codex_stub];
    assert_eq!(
        error_of(&inbox(&fixture, &stub, &env, &configured)),
        pending
    );
    fs::write(
        &toml,
        format!("{before}\n[roles.inbox]\nprovider = \"codex\"\nmodel = \"claude-opus-5-5\"\n"),
    )
    .unwrap();
    let message = error_of(&inbox(&fixture, &stub, &env, &configured));
    assert!(
        message.contains("[roles.inbox]: model claude-opus-5-5 is Claude's"),
        "{message}"
    );
    // A provider with no implementation is a mistake of the setting too,
    // whatever its message names.
    fs::write(
        &toml,
        format!("{before}\n[roles.inbox]\nprovider = \"gemini\"\n"),
    )
    .unwrap();
    let message = error_of(&inbox(&fixture, &stub, &env, &configured));
    assert!(
        message.contains("gemini") && message.ends_with("; the inbox was not opened"),
        "{message}"
    );
    assert_eq!(started(&stub, "argv"), None);
    assert_eq!(opened(&fixture), None);
    // A Claude model named by argument over a Codex table starts Claude
    // with it, and nothing of Codex's.
    fs::write(
        &toml,
        format!("{before}\n[roles.inbox]\nprovider = \"codex\"\n"),
    )
    .unwrap();
    let output = inbox(
        &fixture,
        &stub,
        &env,
        &[
            "--plugin-dir",
            plugin_dir,
            "--provider",
            "claude",
            "--model",
            "claude-sonnet-5-5",
        ],
    );
    assert!(output.status.success(), "{output:?}");
    let argv = started(&stub, "argv").unwrap();
    assert!(
        argv.contains("--model\nclaude-sonnet-5-5\n--effort\nmedium\n--\n"),
        "{argv}"
    );
    assert_eq!(opened(&fixture).unwrap()["provider"], "claude");
}

/// A Codex that cannot be used (ADR-t1181-1 decision 3: its executable
/// does not start, or it is not logged in for the `CODEX_HOME` the inbox
/// would be given) refuses the inbox before the exec with why and the
/// arguments that open it on Claude: nothing is started, Claude included,
/// and nothing is recorded or asked.
#[test]
fn a_codex_that_cannot_be_used_opens_no_inbox_and_says_how_to_open_claude() {
    let fixture = fixture();
    let stub = stub_claude(&fixture);
    let plugin_dir = fixture.options.plugin_dir.clone().unwrap();
    let plugin_dir = plugin_dir.to_str().unwrap();
    let codex_home = fixture._dir.path().join("codex-home");
    fs::create_dir_all(&codex_home).unwrap();
    fs::write(
        codex_home.join("config.toml"),
        "[tui]\nscreen_reader_detection_done = true\n",
    )
    .unwrap();
    let env = [("CODEX_HOME", codex_home.to_str().unwrap())];
    let refused = |codex: &str| {
        error_of(&inbox(
            &fixture,
            &stub,
            &env,
            &[
                "--plugin-dir",
                plugin_dir,
                "--provider",
                "codex",
                "--codex",
                codex,
            ],
        ))
    };
    let instead = format!("; {CLAUDE_INSTEAD}; the inbox was not opened");

    let missing = fixture._dir.path().join("no-codex");
    let message = refused(missing.to_str().unwrap());
    assert!(
        message.starts_with("the Codex inbox was not opened: ")
            && message.contains("could not be started")
            && message.contains(missing.to_str().unwrap())
            && message.ends_with(&instead),
        "{message}"
    );

    let codex = stub_codex(&fixture);
    let message = refused(codex.to_str().unwrap());
    assert!(
        message.starts_with("the Codex inbox was not opened: ")
            && message.contains("is not logged in")
            && message.contains("Not logged in")
            && message.ends_with(&instead),
        "{message}"
    );
    assert_eq!(started(&codex, "home").as_deref(), codex_home.to_str());
    assert_eq!(started(&codex, "argv"), None);
    assert_eq!(started(&stub, "argv"), None);
    assert_eq!(opened(&fixture), None);
    let queue = SqliteQueue::open(&fixture.location.db).unwrap();
    assert_eq!(queue.latest_event_of("ask_opened").unwrap(), None);
}

/// A `dagq.toml` that cannot be read does not keep the default inbox
/// closed: it opens on Claude as before.
#[test]
fn a_dagq_toml_that_cannot_be_read_still_opens_the_default_inbox() {
    let fixture = fixture();
    let stub = stub_claude(&fixture);
    let plugin_dir = fixture.options.plugin_dir.clone().unwrap();
    let toml = fixture.repo.join("dagq.toml");
    let before = fs::read_to_string(&toml).unwrap_or_default();
    fs::write(&toml, format!("{before}\n[no_such_table]\nkey = 1\n")).unwrap();
    let output = inbox(
        &fixture,
        &stub,
        &[],
        &["--plugin-dir", plugin_dir.to_str().unwrap()],
    );
    assert!(output.status.success(), "{output:?}");
    let argv = started(&stub, "argv").unwrap();
    assert!(!argv.contains("--model"), "{argv}");
    assert_eq!(opened(&fixture).unwrap()["provider"], "claude");
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
        codex: PathBuf::from("codex"),
        provider: None,
        model: None,
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
