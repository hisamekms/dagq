//! The Claude Code plugin in `plugins/claude-dagq` is data plus one launcher
//! script and two hook scripts. These tests catch a broken manifest, skill
//! frontmatter, hook, or launcher before `claude plugin validate` or a real
//! session would.

mod common;

use common::{Bounded, WithoutActor};
use dagq::infrastructure::git_binary::git_executable;

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use serde_json::Value;

fn repository_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn plugin_root() -> PathBuf {
    repository_root().join("plugins/claude-dagq")
}

fn plugin_manifest() -> Value {
    serde_json::from_str(
        &fs::read_to_string(plugin_root().join(".claude-plugin/plugin.json")).unwrap(),
    )
    .unwrap()
}

fn skill_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = fs::read_dir(plugin_root().join("skills"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    dirs
}

/// Frontmatter must start on the first line and close with `---`.
fn frontmatter(skill: &str) -> Vec<(String, String)> {
    let mut lines = skill.lines();
    assert_eq!(lines.next(), Some("---"), "frontmatter must open on line 1");
    let mut fields = Vec::new();
    for line in lines {
        if line == "---" {
            return fields;
        }
        let (key, value) = line
            .split_once(':')
            .unwrap_or_else(|| panic!("frontmatter line without key: {line:?}"));
        fields.push((key.trim().to_string(), value.trim().to_string()));
    }
    panic!("frontmatter never closed");
}

#[test]
fn manifest_names_the_plugin_and_tracks_the_crate_version() {
    let manifest = plugin_manifest();
    assert_eq!(manifest["name"], "claude-dagq");
    assert_eq!(manifest["version"], env!("CARGO_PKG_VERSION"));
    assert!(manifest["description"].as_str().unwrap().contains("dagq"));
    // Component paths are optional; if given they must stay inside the plugin.
    for key in ["skills", "commands", "agents", "hooks"] {
        if let Some(path) = manifest[key].as_str() {
            assert!(
                path.starts_with("./") && !path.contains(".."),
                "{key}: {path}"
            );
        }
    }
}

#[test]
fn every_skill_has_valid_frontmatter_and_uses_the_launcher() {
    let dirs = skill_dirs();
    let names: Vec<String> = dirs
        .iter()
        .map(|d| d.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names,
        ["dagq", "dagq-inbox", "dagq-planner", "dagq-recover"]
    );
    for dir in &dirs {
        let skill = fs::read_to_string(dir.join("SKILL.md")).unwrap();
        // A skill is reloaded on every use and after each compaction, so its
        // body stays small; lists of fields and states live in reference/.
        assert!(
            skill.len() <= 8 * 1024,
            "{}: SKILL.md is {} bytes",
            dir.display(),
            skill.len()
        );
        let fields = frontmatter(&skill);
        let get = |key: &str| {
            fields
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
        };
        let name = get("name").expect("name");
        assert_eq!(name, dir.file_name().unwrap().to_string_lossy());
        assert!(
            name.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
            "{name} must be kebab-case"
        );
        let description = get("description").expect("description");
        assert!(
            (40..=1024).contains(&description.len()),
            "{name}: description length {}",
            description.len()
        );
        assert!(
            skill.contains("${CLAUDE_PLUGIN_ROOT}/bin/dagq"),
            "{name} must call the launcher"
        );
        assert!(
            !skill.contains("sqlite3 "),
            "{name} must not open the database directly"
        );
    }
}

/// Every `reference/<file>.md` a skill names exists (in its own directory,
/// or in the skill a `skills/<name>/reference/` path names), and every file
/// under a skill's `reference/` is named by its SKILL.md, so none is
/// unreachable.
#[test]
fn skills_point_at_their_reference_files() {
    let mut with_reference = Vec::new();
    for dir in skill_dirs() {
        let name = dir.file_name().unwrap().to_string_lossy().into_owned();
        let skill = fs::read_to_string(dir.join("SKILL.md")).unwrap();
        let reference = dir.join("reference");
        if reference.is_dir() {
            with_reference.push(name.clone());
            for entry in fs::read_dir(&reference).unwrap() {
                let file = entry.unwrap().file_name().to_string_lossy().into_owned();
                assert!(file.ends_with(".md"), "{name}: {file}");
                assert!(skill.contains(&file), "{name} never names reference/{file}");
            }
        }
        for (index, _) in skill.match_indices("reference/") {
            let file: String = skill[index + "reference/".len()..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '.')
                .collect();
            let file = file.trim_end_matches('.');
            let owner = skill[..index]
                .strip_suffix('/')
                .and_then(|before| before.rsplit_once("skills/"))
                .map(|(_, other)| other)
                .filter(|other| !other.contains('/'))
                .map_or(reference.clone(), |other| {
                    plugin_root().join("skills").join(other).join("reference")
                });
            assert!(
                owner.join(file).is_file(),
                "{name}: {}",
                owner.join(file).display()
            );
        }
    }
    with_reference.sort();
    assert_eq!(with_reference, ["dagq", "dagq-inbox", "dagq-recover"]);
    // The watch never lands a run on its own, and it is one command with no
    // timeout, never a shell loop around it (task 943).
    let inbox = fs::read_to_string(plugin_root().join("skills/dagq-inbox/SKILL.md")).unwrap();
    assert!(inbox.contains("\"$DAGQ\" watch --role inbox --until-attention --after <cursor>"));
    assert!(inbox.contains("run_in_background"));
    assert!(!inbox.contains("\"$DAGQ\" integrate"));
    let watch =
        fs::read_to_string(plugin_root().join("skills/dagq-inbox/reference/watch.md")).unwrap();
    assert!(watch.contains("\"$DAGQ\" watch --role inbox --until-attention --after <cursor>"));
    assert!(watch.contains("run_in_background"));
    for loop_part in ["jq", "while :", "sh -c", "empty timeout"] {
        assert!(!inbox.contains(loop_part), "dagq-inbox has {loop_part}");
        assert!(!watch.contains(loop_part), "watch.md has {loop_part}");
    }
    let review =
        fs::read_to_string(plugin_root().join("skills/dagq-recover/reference/review-by-hand.md"))
            .unwrap();
    assert!(review.contains("\"$DAGQ\" review ID"));
    assert!(review.contains("On `pass` and their word, `\"$DAGQ\" integrate ID`"));
    assert!(review.contains("--option land --option send_back --option cancel"));
    assert!(review.contains("git push origin main"));
}

/// Where a word like `task` is followed by its number (`task 528`,
/// `ask #160`), outside a flag's placeholder (`--goal 1`).
fn numbered_anecdote(text: &str) -> Option<&str> {
    const WORDS: [&str; 8] = [
        "task", "goal", "ask", "proposal", "finding", "note", "run", "mark",
    ];
    let lower = text.to_ascii_lowercase();
    for word in WORDS {
        for (start, _) in lower.match_indices(word) {
            let before = lower[..start].chars().next_back();
            if before.is_some_and(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
                continue;
            }
            let rest = lower[start + word.len()..]
                .strip_prefix('s')
                .unwrap_or(&lower[start + word.len()..]);
            let Some(rest) = rest.strip_prefix(' ') else {
                continue;
            };
            let rest = rest.strip_prefix('#').unwrap_or(rest);
            if rest.starts_with(|c: char| c.is_ascii_digit()) {
                let end = text.len() - rest.len()
                    + rest
                        .find(|c: char| !c.is_ascii_digit())
                        .unwrap_or(rest.len());
                return Some(&text[start..end]);
            }
        }
    }
    None
}

/// Where a date (`2026-09-26`) stands.
fn dated(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    (0..bytes.len().saturating_sub(9)).find_map(|start| {
        let date = &bytes[start..start + 10];
        let digits = |range: std::ops::Range<usize>| date[range].iter().all(u8::is_ascii_digit);
        (date.starts_with(b"20")
            && digits(0..4)
            && date[4] == b'-'
            && digits(5..7)
            && date[7] == b'-'
            && digits(8..10))
        .then(|| &text[start..start + 10])
    })
}

/// ADR-t1453-2 decision 1: the plugin is dagq's generic procedure for any
/// repository and refers to that repository's rules, so no skill or
/// reference carries this repository's rules, values or history. The marks:
/// this repository's check and test commands (the coverage gate and its
/// threshold, the test binaries and helpers, which paths take the runtime
/// checks), its `[areas]` name, its host tools, the old ADR file form,
/// wording that speaks for "this repository", a task's, goal's or ask's
/// number told as an anecdote, and a date. Where each rule lives now is in
/// `docs/plans/agents-slim-inventory.md` section 4.
#[test]
fn no_skill_carries_this_repository_s_rules() {
    assert_eq!(
        numbered_anecdote("decided in task 528 by"),
        Some("task 528")
    );
    assert_eq!(numbered_anecdote("(Asks #160)"), Some("Asks #160"));
    assert_eq!(
        numbered_anecdote("add --goal 1 and --depends-on-goal 2"),
        None
    );
    assert_eq!(numbered_anecdote("ask <id>, the task's goal"), None);
    assert_eq!(dated("on 2026-09-26 (median"), Some("2026-09-26"));
    assert_eq!(dated("version 0.4.0-dev"), None);
    const MARKS: [&str; 21] = [
        "llvm-cov",
        "nextest",
        "fail-under-lines",
        "tests/it",
        "tests/common",
        "tests/plugin.rs",
        "--test it",
        "--test plugin",
        "cargo fmt",
        "clippy",
        "RUSTC_WRAPPER",
        "sccache",
        "docs/adr/NNNN",
        "runtime checks",
        "--area runtime",
        "area=runtime",
        "For this repository",
        "for this repository",
        "in this repository",
        "In this repository",
        "this repository's AGENTS.md",
    ];
    let mut files = Vec::new();
    let mut dirs = vec![plugin_root().join("skills")];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.extension().is_some_and(|extension| extension == "md") {
                files.push(path);
            }
        }
    }
    assert!(files.len() > 4, "{files:?}");
    let mut found = Vec::new();
    for file in files {
        let text = fs::read_to_string(&file).unwrap();
        let name = file
            .strip_prefix(plugin_root())
            .unwrap()
            .display()
            .to_string();
        for (number, line) in text.lines().enumerate() {
            let at = |what: &str| format!("{name}:{}: {what}", number + 1);
            found.extend(
                MARKS
                    .iter()
                    .filter(|mark| line.contains(*mark))
                    .map(|mark| at(mark)),
            );
            found.extend(numbered_anecdote(line).map(at));
            found.extend(dated(line).map(at));
        }
    }
    assert!(
        found.is_empty(),
        "this repository's rules in the plugin: {found:#?}"
    );
}

/// ADR-0024: the roles are the supervisor, the worker, the planner, the
/// inbox and the observer. Every attention reaches the person through the
/// inbox, which decides nothing and acts only on the person's word through
/// `dagq-recover`; registering goals and tasks is the planner's, and a goal
/// review job, not the planner, judges and closes a finished goal.
#[test]
fn skills_split_the_roles_of_inbox_planner_and_recover() {
    let read = |name: &str| {
        fs::read_to_string(plugin_root().join(format!("skills/{name}/SKILL.md"))).unwrap()
    };
    for name in ["dagq-inbox", "dagq-recover"] {
        let skill = read(name);
        for forbidden in [
            "\"$DAGQ\" goal close",
            "\"$DAGQ\" goal add",
            "\"$DAGQ\" add ",
        ] {
            assert!(!skill.contains(forbidden), "{name} mentions {forbidden}");
        }
    }
    for dir in skill_dirs() {
        let skill = fs::read_to_string(dir.join("SKILL.md")).unwrap();
        assert!(
            !skill.to_lowercase().contains("maintain"),
            "{}",
            dir.display()
        );
    }

    let inbox = read("dagq-inbox");
    assert!(inbox.contains("\"$DAGQ\" status --role inbox"));
    assert!(inbox.contains("\"$DAGQ\" asks --open --role inbox"));
    assert!(inbox.contains("\"$DAGQ\" answer <id> --text"));
    assert!(inbox.contains("Add no recommendation of your own"));
    assert!(inbox.contains("Only when the person says so"));
    for next in [
        "`read the answer of ask <id> and close it`",
        "`restart supervisor`",
        "`review by hand`",
        "`push main`",
        "`triage by hand`",
        "`send the answer of ask <id> to the worker and close it`",
        "`check the e2e host`",
        "`stop the dead landing's processes`",
        "`dagq service status`",
        "`dagq broker status`",
    ] {
        assert!(inbox.contains(next), "dagq-inbox lacks {next}");
    }
    assert!(inbox.contains("skills/dagq-recover/reference/session.md"));
    assert!(inbox.contains("reference/status.md"));

    let recover = read("dagq-recover");
    for section in [
        "## 3. Recover (only without a supervisor)",
        "## 4. Triage by hand",
        "## 5. Start, stop and update the runtime",
        "## 6. Review by hand, and a failed push",
        "## 7. A run's session",
    ] {
        assert!(recover.contains(section), "dagq-recover lacks {section}");
    }
    assert!(recover.contains("\"$DAGQ\" up"));
    assert!(
        !recover.contains("--plugin-dir \"$CLAUDE_PLUGIN_ROOT\""),
        "dagq-recover must not pass the installed plugin's per-version cache to --plugin-dir"
    );
    assert!(recover.contains("\"$DAGQ\" down --wait"));
    let stuck_exit =
        fs::read_to_string(plugin_root().join("skills/dagq-recover/reference/stuck-exit.md"))
            .unwrap();
    for step in [
        "\"Background work is running\"",
        "git -C <worktree_path> status --porcelain",
        "`jq -r .commit <receipt_path>` equals `git -C <worktree_path> rev-parse HEAD`",
        "select \"Exit and stop tasks\"",
        "## Answer `wait`, or anything else",
        "\"$DAGQ\" asks --role inbox",
    ] {
        assert!(stuck_exit.contains(step), "stuck-exit.md lacks {step}");
    }

    let planner = read("dagq-planner");
    assert!(planner.contains("skills/dagq/SKILL.md"));
    assert!(planner.contains("skills/dagq/reference/goal-close.md"));
    assert!(planner.contains("skills/dagq/reference/observer.md"));
    assert!(planner.contains("skills/dagq-recover/SKILL.md"));
    assert!(planner.contains("follow_ups"));
    assert!(planner.contains("Never: `integrate`, `review`, `answer`"));
    // A goal review job, not the planner, judges and closes a finished goal
    // (ADR-0047 decisions 16 and 43).
    let finished_goal = planner
        .split("## 5. A finished goal")
        .nth(1)
        .and_then(|rest| rest.split("\n## ").next())
        .expect("dagq-planner lacks ## 5. A finished goal");
    for duty in ["**goal review** job", "`approve_goal`", "`goal_gap`"] {
        assert!(
            finished_goal.contains(duty),
            "dagq-planner section 5 lacks {duty}"
        );
    }
    assert!(inbox.contains("`goal review by hand`"));
    assert!(recover.contains("reference/goal-review-by-hand.md"));
    assert!(
        plugin_root()
            .join("skills/dagq-recover/reference/goal-review-by-hand.md")
            .is_file()
    );
    let dagq = read("dagq");
    assert!(dagq.contains("judged by the supervisor's goal review job"));
}

/// The runtime resumes `needs_session` runs (ADR-0019): no skill or
/// reference file tells a session to open a resume session itself.
#[test]
fn no_skill_opens_a_resume_session() {
    for dir in skill_dirs() {
        let mut files = vec![dir.join("SKILL.md")];
        if let Ok(entries) = fs::read_dir(dir.join("reference")) {
            files.extend(entries.map(|entry| entry.unwrap().path()));
        }
        for file in files {
            let text = fs::read_to_string(&file).unwrap();
            for forbidden in ["claude --resume", "workspace create"] {
                assert!(
                    !text.contains(forbidden),
                    "{} mentions {forbidden}",
                    file.display()
                );
            }
        }
    }
    let recover = fs::read_to_string(plugin_root().join("skills/dagq-recover/SKILL.md")).unwrap();
    assert!(recover.contains("a `needs_session` run is resumed (`resuming (runtime)`)"));
    assert!(recover.contains("never open a resume workspace yourself"));
}

fn hooks_manifest() -> Value {
    serde_json::from_str(&fs::read_to_string(plugin_root().join("hooks/hooks.json")).unwrap())
        .unwrap()
}

#[test]
fn hooks_json_runs_the_status_on_every_start_the_watch_check_on_stop_and_records_every_start_and_end()
 {
    let hooks = hooks_manifest();
    let events = hooks["hooks"].as_object().expect("hooks object");
    let mut names: Vec<&String> = events.keys().collect();
    names.sort();
    assert_eq!(names, ["SessionEnd", "SessionStart", "Stop"]);
    let command = |group: &Value| -> String {
        let commands = group["hooks"].as_array().unwrap();
        assert_eq!(commands.len(), 1, "{group}");
        assert_eq!(commands[0]["type"], "command");
        commands[0]["command"].as_str().unwrap().to_owned()
    };
    let groups = events["SessionStart"].as_array().unwrap();
    assert_eq!(groups.len(), 2);
    // The status: every source (ADR-t906-1); the script leaves a planner's
    // startup and resume alone.
    assert!(groups[0].get("matcher").is_none(), "{}", groups[0]);
    assert_eq!(
        command(&groups[0]),
        "\"${CLAUDE_PLUGIN_ROOT}/hooks/session-start.sh\""
    );
    // The span: every source (no matcher), and every end.
    assert!(groups[1].get("matcher").is_none(), "{}", groups[1]);
    assert_eq!(
        command(&groups[1]),
        "\"${CLAUDE_PLUGIN_ROOT}/hooks/session-event.sh\" open"
    );
    let ends = events["SessionEnd"].as_array().unwrap();
    assert_eq!(ends.len(), 1);
    assert!(ends[0].get("matcher").is_none(), "{}", ends[0]);
    assert_eq!(
        command(&ends[0]),
        "\"${CLAUDE_PLUGIN_ROOT}/hooks/session-event.sh\" close"
    );
    let stops = events["Stop"].as_array().unwrap();
    assert_eq!(stops.len(), 1);
    assert!(stops[0].get("matcher").is_none(), "{}", stops[0]);
    assert_eq!(
        command(&stops[0]),
        "\"${CLAUDE_PLUGIN_ROOT}/hooks/stop-watch.sh\""
    );
    for script in ["session-start.sh", "session-event.sh", "stop-watch.sh"] {
        let mode = fs::metadata(plugin_root().join("hooks").join(script))
            .unwrap()
            .permissions()
            .mode();
        assert_ne!(mode & 0o111, 0, "{script} must be executable");
    }
}

/// Claude Code runs a hook's `command` with `sh -c` after putting the plugin's
/// path in place of `${CLAUDE_PLUGIN_ROOT}`, so every command quotes it: a
/// plugin directory with a space still runs the script with its arguments.
#[test]
fn hook_commands_run_from_a_plugin_directory_with_a_space() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("plugin root");
    fs::create_dir_all(root.join("hooks")).unwrap();
    fs::create_dir_all(root.join("bin")).unwrap();
    for script in ["session-start.sh", "session-event.sh", "stop-watch.sh"] {
        fs::copy(
            plugin_root().join("hooks").join(script),
            root.join("hooks").join(script),
        )
        .unwrap();
    }
    // A stand-in launcher that records its arguments.
    let log = root.join("calls.log");
    let stub = root.join("bin/dagq");
    crate::common::template::script_env(
        &stub,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$STUB_PATH\"\n",
        &[("STUB_PATH", log.to_str().unwrap())],
    );

    let hooks = hooks_manifest();
    let mut commands = Vec::new();
    for groups in hooks["hooks"].as_object().unwrap().values() {
        for group in groups.as_array().unwrap() {
            for hook in group["hooks"].as_array().unwrap() {
                commands.push(hook["command"].as_str().unwrap().to_owned());
            }
        }
    }
    assert_eq!(commands.len(), 4);
    for command in &commands {
        let expanded = command.replace("${CLAUDE_PLUGIN_ROOT}", root.to_str().unwrap());
        let output = Command::new("sh")
            .args(["-c", &expanded])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("CLAUDE_PLUGIN_ROOT", &root)
            .env("DAGQ_ROLE", "inbox")
            .env("DAGQ_QUEUE", dir.path().join("queue.db"))
            .env("DAGQ_BIN", &stub)
            .current_dir(dir.path())
            .stdin(std::process::Stdio::null())
            .bounded_output()
            .unwrap();
        assert!(
            output.status.success(),
            "{command}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stderr, b"", "{command}");
    }
    let mut calls: Vec<String> = fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    calls.sort();
    assert_eq!(
        calls,
        [
            "session-event close",
            "session-event open",
            // The SessionStart status and the Stop hook's, whose empty
            // output has no watcher to judge.
            "status --role inbox",
            "status --role inbox"
        ]
    );
}

/// Runs the span hook for `event` with a clean environment plus `env`, the
/// hook's JSON `input` on stdin.
fn session_event(
    event: &str,
    input: &Value,
    env: &[(&str, &str)],
    data_home: &Path,
    cwd: &Path,
) -> Output {
    use std::io::Write;
    let mut command = Command::new(plugin_root().join("hooks/session-event.sh"));
    command
        .arg(event)
        .env_clear()
        .env("XDG_DATA_HOME", data_home)
        .env("PATH", "/usr/bin:/bin")
        .current_dir(cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for (key, value) in env {
        command.env(key, value);
    }
    let _waiting = common::within(common::STEP_LIMIT, "the session-event hook to exit");
    let mut child = command.spawn().unwrap();
    // A hook that exits without reading its input closes the pipe first.
    let _ = child
        .stdin
        .take()
        .unwrap()
        .write_all(input.to_string().as_bytes());
    child.wait_with_output().unwrap()
}

/// The span events of the queue at `db`, oldest first.
fn span_events(binary: &str, db: &str, data_home: &Path, cwd: &Path) -> Vec<Value> {
    let events = stdout_json(&launcher(
        &[("DAGQ_BIN", binary), ("DAGQ_DB", db)],
        data_home,
        cwd,
        &[
            "events",
            "--full",
            "--kind",
            "session_opened",
            "--kind",
            "session_closed",
            "--limit",
            "100",
        ],
    ));
    events["events"].as_array().unwrap().clone()
}

#[test]
fn session_event_hook_records_the_spans_of_the_inbox_and_planners_only() {
    let dir = tempfile::tempdir().unwrap();
    let data_home = dir.path().join("xdg");
    let repo = dir.path().join("repo");
    fs::create_dir(&repo).unwrap();
    assert!(
        Command::new(git_executable().expect("git executable"))
            .args(["init", "-q", "-b", "main"])
            .current_dir(&repo)
            .bounded_status()
            .unwrap()
            .success()
    );
    let binary = env!("CARGO_BIN_EXE_dagq");
    stdout_json(&launcher(
        &[("DAGQ_BIN", binary)],
        &data_home,
        &repo,
        &["init"],
    ));
    let db = stdout_json(&launcher(
        &[("DAGQ_BIN", binary)],
        &data_home,
        &repo,
        &["locate"],
    ))["db"]
        .as_str()
        .unwrap()
        .to_string();
    let start = |session: &str, source: &str| {
        serde_json::json!({
            "session_id": session,
            "transcript_path": dir.path().join(format!("{session}.jsonl")),
            "cwd": repo,
            "hook_event_name": "SessionStart",
            "source": source,
        })
    };
    let end = |session: &str, reason: &str| serde_json::json!({"session_id": session, "hook_event_name": "SessionEnd", "reason": reason});
    let silent = |output: Output| {
        assert!(output.status.success(), "{output:?}");
        assert_eq!(output.stdout, b"", "{output:?}");
        assert_eq!(output.stderr, b"", "{output:?}");
    };

    // Workers and sessions of no role or no queue record nothing.
    for env in [
        vec![("DAGQ_BIN", binary), ("DAGQ_QUEUE", db.as_str())],
        vec![
            ("DAGQ_BIN", binary),
            ("DAGQ_ROLE", "worker"),
            ("DAGQ_QUEUE", db.as_str()),
        ],
        vec![("DAGQ_BIN", binary), ("DAGQ_ROLE", "inbox")],
    ] {
        silent(session_event(
            "open",
            &start("s-0", "startup"),
            &env,
            &data_home,
            dir.path(),
        ));
    }
    assert_eq!(
        span_events(binary, &db, &data_home, &repo),
        Vec::<Value>::new()
    );

    // The inbox: opened at its start, gone on with at a compaction, closed
    // and replaced at a /clear, and closed at its end, once.
    let inbox = [
        ("DAGQ_BIN", binary),
        ("DAGQ_ROLE", "inbox"),
        ("DAGQ_SESSION_KIND", "inbox"),
        ("DAGQ_QUEUE", db.as_str()),
        ("CMUX_WORKSPACE_ID", "W-INBOX"),
    ];
    silent(session_event(
        "open",
        &start("s-1", "startup"),
        &inbox,
        &data_home,
        dir.path(),
    ));
    silent(session_event(
        "open",
        &start("s-1", "compact"),
        &inbox,
        &data_home,
        dir.path(),
    ));
    silent(session_event(
        "close",
        &end("s-1", "clear"),
        &inbox,
        &data_home,
        dir.path(),
    ));
    silent(session_event(
        "open",
        &start("s-2", "clear"),
        &inbox,
        &data_home,
        dir.path(),
    ));
    silent(session_event(
        "close",
        &end("s-2", "prompt_input_exit"),
        &inbox,
        &data_home,
        dir.path(),
    ));
    silent(session_event(
        "close",
        &end("s-2", "other"),
        &inbox,
        &data_home,
        dir.path(),
    ));
    // A planner of an old workspace (no DAGQ_SESSION_KIND) whose SessionEnd
    // was lost: its /clear closes it as the next span.
    let planner = [
        ("DAGQ_BIN", binary),
        ("DAGQ_ROLE", "planner"),
        ("DAGQ_QUEUE", db.as_str()),
        ("CMUX_WORKSPACE_ID", "W-PLANNER"),
    ];
    silent(session_event(
        "open",
        &start("p-1", "startup"),
        &planner,
        &data_home,
        dir.path(),
    ));
    silent(session_event(
        "open",
        &start("p-2", "clear"),
        &planner,
        &data_home,
        dir.path(),
    ));

    let spans: Vec<(String, String, String)> = span_events(binary, &db, &data_home, &repo)
        .iter()
        .map(|event| {
            let payload = &event["payload"];
            (
                event["kind"].as_str().unwrap().to_owned(),
                format!(
                    "{} {}",
                    payload["kind"].as_str().unwrap(),
                    payload["session_id"].as_str().unwrap()
                ),
                payload["reason"]
                    .as_str()
                    .or(payload["source"].as_str())
                    .unwrap()
                    .to_owned(),
            )
        })
        .collect();
    let row =
        |kind: &str, span: &str, why: &str| (kind.to_owned(), span.to_owned(), why.to_owned());
    assert_eq!(
        spans,
        [
            row("session_opened", "inbox s-1", "startup"),
            row("session_closed", "inbox s-1", "clear"),
            row("session_opened", "inbox s-2", "clear"),
            row("session_closed", "inbox s-2", "exited"),
            row("session_opened", "planner p-1", "startup"),
            row("session_closed", "planner p-1", "next_span"),
            row("session_opened", "planner p-2", "clear"),
        ]
    );
    let events = span_events(binary, &db, &data_home, &repo);
    assert_eq!(events[0]["payload"]["workspace_id"], "W-INBOX");
    assert_eq!(events[0]["task_id"], Value::Null);
    // Hook closes first; the supervisor has not attempted intake yet.
    assert_eq!(events[1]["payload"]["active"], "unavailable");
    assert_eq!(
        events[1]["payload"]["active_unavailable"],
        "hook_intake_pending"
    );

    // stats counts them in its window, per kind.
    let stats = stdout_json(&launcher(
        &[("DAGQ_BIN", binary), ("DAGQ_DB", db.as_str())],
        &data_home,
        &repo,
        &["stats", "--full"],
    ));
    let by_kind = &stats["sessions"]["by_kind"];
    assert_eq!(by_kind["inbox"]["count"], 2, "{by_kind}");
    assert_eq!(by_kind["inbox"]["open_now"], 0, "{by_kind}");
    assert_eq!(by_kind["planner"]["count"], 2, "{by_kind}");
    assert_eq!(by_kind["planner"]["open_now"], 1, "{by_kind}");

    // A failure is silent and never fails the session: no dagq, a dagq
    // that is not executable, a queue that cannot be opened, bad input.
    silent(session_event(
        "open",
        &start("s-3", "startup"),
        &[("DAGQ_ROLE", "inbox"), ("DAGQ_QUEUE", db.as_str())],
        &data_home,
        dir.path(),
    ));
    let bogus = dir.path().join("not-executable");
    fs::write(&bogus, "").unwrap();
    silent(session_event(
        "open",
        &start("s-3", "startup"),
        &[
            ("DAGQ_BIN", bogus.to_str().unwrap()),
            ("DAGQ_ROLE", "inbox"),
            ("DAGQ_QUEUE", db.as_str()),
        ],
        &data_home,
        dir.path(),
    ));
    let missing = dir.path().join("missing").join("queue.db");
    silent(session_event(
        "open",
        &start("s-3", "startup"),
        &[
            ("DAGQ_BIN", binary),
            ("DAGQ_ROLE", "inbox"),
            ("DAGQ_QUEUE", missing.to_str().unwrap()),
        ],
        &data_home,
        dir.path(),
    ));
    silent(session_event(
        "open",
        &serde_json::json!({"source": "startup"}),
        &inbox,
        &data_home,
        dir.path(),
    ));
    silent(session_event(
        "bogus",
        &start("s-3", "startup"),
        &inbox,
        &data_home,
        dir.path(),
    ));
    assert_eq!(span_events(binary, &db, &data_home, &repo).len(), 7);
}

/// Runs the SessionStart hook with a clean environment plus `env`, and no
/// input.
fn session_start(env: &[(&str, &str)], data_home: &Path, cwd: &Path) -> Output {
    let mut command = Command::new(plugin_root().join("hooks/session-start.sh"));
    command
        .env_clear()
        .env("XDG_DATA_HOME", data_home)
        .env("PATH", "/usr/bin:/bin")
        .current_dir(cwd);
    for (key, value) in env {
        command.env(key, value);
    }
    command.bounded_output().unwrap()
}

/// Runs `script` of the plugin's hooks with a clean environment plus `env`,
/// the hook's JSON `input` on stdin.
fn hook_with_input(
    script: &str,
    input: &Value,
    env: &[(&str, &str)],
    data_home: &Path,
    cwd: &Path,
) -> Output {
    use std::io::Write;
    let mut command = Command::new(plugin_root().join("hooks").join(script));
    command
        .env_clear()
        .env("XDG_DATA_HOME", data_home)
        .env("PATH", "/usr/bin:/bin")
        .current_dir(cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for (key, value) in env {
        command.env(key, value);
    }
    let _waiting = common::within(common::STEP_LIMIT, format!("the {script} hook to exit"));
    let mut child = command.spawn().unwrap();
    // A hook that exits without reading its input closes the pipe first.
    let _ = child
        .stdin
        .take()
        .unwrap()
        .write_all(input.to_string().as_bytes());
    child.wait_with_output().unwrap()
}

/// Splits the hook's stdout for `role` into its leading line of text, which
/// keeps Claude Code from reading the output as the hook's control JSON, and
/// the status JSON after it. The inbox's output starts with one more line,
/// which makes the watch from the status's cursor its first move.
fn hook_status(output: &Output, role: &str) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stderr, b"");
    let text = String::from_utf8(output.stdout.clone()).unwrap();
    let (first, rest) = text.split_once('\n').expect("a header line");
    let (watch_line, (header, status)) = if role == "inbox" {
        (Some(first), rest.split_once('\n').expect("a header line"))
    } else {
        (None, (first, rest))
    };
    assert!(
        serde_json::from_str::<Value>(&text).is_err(),
        "the whole stdout must not parse as JSON: {text}"
    );
    if let Some(line) = watch_line {
        let cursor = serde_json::from_str::<Value>(status).unwrap()["cursor"]
            .as_i64()
            .unwrap();
        assert!(
            line.starts_with("First move, before anything else: start the watch of"),
            "{line}"
        );
        assert!(
            line.contains(&format!(
                "watch --role inbox --until-attention --after {cursor} with no shell loop"
            )),
            "{line}"
        );
    }
    assert_eq!(
        header,
        format!(
            "This session is the dagq {role} (DAGQ_ROLE={role}); follow the dagq-{role} skill of the dagq plugin. The queue status for this role (dagq status --role {role}); when its language.instruction is set, write for people as it says:"
        )
    );
    serde_json::from_str(status).unwrap()
}

#[test]
fn session_start_hook_prints_status_only_in_the_sessions_up_opens() {
    let dir = tempfile::tempdir().unwrap();
    let data_home = dir.path().join("xdg");
    let repo = dir.path().join("repo");
    fs::create_dir(&repo).unwrap();
    assert!(
        Command::new(git_executable().expect("git executable"))
            .args(["init", "-q", "-b", "main"])
            .current_dir(&repo)
            .bounded_status()
            .unwrap()
            .success()
    );
    let binary = env!("CARGO_BIN_EXE_dagq");
    stdout_json(&launcher(
        &[("DAGQ_BIN", binary)],
        &data_home,
        &repo,
        &["init"],
    ));

    // No role, or another role: nothing at all, whatever else is set.
    for env in [
        vec![("DAGQ_BIN", binary)],
        vec![("DAGQ_BIN", binary), ("DAGQ_ROLE", "worker")],
        vec![("DAGQ_BIN", binary), ("DAGQ_ROLE", "observer")],
        vec![("DAGQ_BIN", binary), ("DAGQ_ROLE", "supervisor")],
        vec![("DAGQ_BIN", binary), ("DAGQ_ROLE", "inboxes")],
        vec![("DAGQ_ROLE", "")],
    ] {
        let output = session_start(&env, &data_home, &repo);
        assert!(output.status.success(), "{env:?}");
        assert_eq!(output.stdout, b"", "{env:?}");
        assert_eq!(output.stderr, b"", "{env:?}");
    }

    // The inbox gets status, with all the attention and the cursor.
    let inbox_env = [("DAGQ_BIN", binary), ("DAGQ_ROLE", "inbox")];
    let status = hook_status(&session_start(&inbox_env, &data_home, &repo), "inbox");
    assert!(status["supervisors"].is_array());
    assert!(status["attention"].is_array());
    assert_eq!(status["attention"][0]["kind"], "supervisor_stopped");
    assert!(status["cursor"].is_number());
    assert_eq!(status["inbox_watcher"]["watching"], 0, "{status}");

    // The inbox gets the watch line and the status on every source
    // (ADR-t906-1); a planner's startup and resume get nothing.
    let planner_env = [("DAGQ_BIN", binary), ("DAGQ_ROLE", "planner")];
    for source in ["startup", "resume", "clear", "compact"] {
        let input = serde_json::json!({
            "session_id": "s",
            "hook_event_name": "SessionStart",
            "source": source,
        });
        let inbox = hook_status(
            &hook_with_input("session-start.sh", &input, &inbox_env, &data_home, &repo),
            "inbox",
        );
        assert!(inbox["cursor"].is_number(), "{source}");
        let planner = hook_with_input("session-start.sh", &input, &planner_env, &data_home, &repo);
        if matches!(source, "startup" | "resume") {
            assert!(planner.status.success(), "{source}");
            assert_eq!(planner.stdout, b"", "{source}");
            assert_eq!(planner.stderr, b"", "{source}");
        } else {
            assert!(hook_status(&planner, "planner")["cursor"].is_number());
        }
    }

    // The inbox and the planner get the status of their role: every
    // attention is the inbox's, none the planner's.
    stdout_json(&launcher(
        &[("DAGQ_BIN", binary), ("PATH", "/usr/bin:/bin")],
        &data_home,
        &repo,
        &[
            "ask",
            "--kind",
            "blocked",
            "--because",
            "scope",
            "--question",
            "stuck?",
            "--option",
            "wait",
            "--recommend",
            "wait",
            "--cmux",
            "/usr/bin/true",
        ],
    ));
    let inbox = hook_status(
        &session_start(
            &[("DAGQ_BIN", binary), ("DAGQ_ROLE", "inbox")],
            &data_home,
            &repo,
        ),
        "inbox",
    );
    let kinds = |status: &Value| -> Vec<String> {
        status["attention"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["kind"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(kinds(&inbox), ["supervisor_stopped", "ask_opened"]);
    assert_eq!(inbox["asks"][0]["question"], "stuck?");
    assert!(inbox["cursor"].is_number());
    let planner = hook_status(
        &session_start(
            &[("DAGQ_BIN", binary), ("DAGQ_ROLE", "planner")],
            &data_home,
            &repo,
        ),
        "planner",
    );
    assert!(kinds(&planner).is_empty(), "{planner}");
    assert!(planner["cursor"].is_number());

    // The status the hook prints carries the language the user set, and
    // the instruction the session follows (ADR-t616-2).
    assert_eq!(planner["language"]["source"], "unset", "{planner}");
    let config_home = dir.path().join("config");
    fs::create_dir_all(config_home.join("dagq")).unwrap();
    fs::write(
        config_home.join("dagq/config.toml"),
        "[language]\ntag = \"ja\"\n",
    )
    .unwrap();
    let planner = hook_status(
        &session_start(
            &[
                ("DAGQ_BIN", binary),
                ("DAGQ_ROLE", "planner"),
                ("XDG_CONFIG_HOME", config_home.to_str().unwrap()),
            ],
            &data_home,
            &repo,
        ),
        "planner",
    );
    assert_eq!(planner["language"]["tag"], "ja", "{planner}");
    assert_eq!(planner["language"]["source"], "user", "{planner}");
    assert!(
        planner["language"]["instruction"]
            .as_str()
            .unwrap()
            .contains("BCP 47 tag `ja`"),
        "{planner}"
    );

    // `up` names the queue in DAGQ_QUEUE, which works outside the repository.
    let db = stdout_json(&launcher(
        &[("DAGQ_BIN", binary)],
        &data_home,
        &repo,
        &["locate"],
    ))["db"]
        .as_str()
        .unwrap()
        .to_string();
    let with_queue = [
        ("DAGQ_BIN", binary),
        ("DAGQ_ROLE", "planner"),
        ("DAGQ_QUEUE", db.as_str()),
    ];
    let status = hook_status(
        &session_start(&with_queue, &data_home, dir.path()),
        "planner",
    );
    assert!(status["cursor"].is_number());

    // A failure is one line of explanation, never a failed session start.
    let one_line = |output: &Output| {
        assert!(output.status.success());
        let text = String::from_utf8(output.stdout.clone()).unwrap();
        assert_eq!(text.lines().count(), 1, "{text}");
        text
    };
    let missing = one_line(&session_start(&[("DAGQ_ROLE", "inbox")], &data_home, &repo));
    assert!(missing.contains("dagq was not found"), "{missing}");
    let bogus = dir.path().join("not-executable");
    fs::write(&bogus, "").unwrap();
    let bad = one_line(&session_start(
        &[
            ("DAGQ_ROLE", "inbox"),
            ("DAGQ_BIN", bogus.to_str().unwrap()),
        ],
        &data_home,
        &repo,
    ));
    assert!(bad.contains("not an executable"), "{bad}");
    let outside = one_line(&session_start(&inbox_env, &data_home, dir.path()));
    assert!(outside.starts_with("dagq status failed: "), "{outside}");
}

#[test]
fn stop_hook_blocks_only_an_inbox_without_a_watch() {
    let dir = tempfile::tempdir().unwrap();
    let data_home = dir.path().join("xdg");
    let db = dir.path().join("queue.db");
    let db = db.to_str().unwrap();
    let binary = env!("CARGO_BIN_EXE_dagq");
    let with_db = [("DAGQ_BIN", binary), ("DAGQ_DB", db)];
    stdout_json(&launcher(&with_db, &data_home, dir.path(), &["init"]));
    let stop = |active: bool, env: &[(&str, &str)]| {
        hook_with_input(
            "stop-watch.sh",
            &serde_json::json!({
                "session_id": "s",
                "hook_event_name": "Stop",
                "stop_hook_active": active,
            }),
            env,
            &data_home,
            dir.path(),
        )
    };
    let silent = |output: &Output, case: &str| {
        assert!(output.status.success(), "{case}");
        assert_eq!(output.stdout, b"", "{case}");
        assert_eq!(output.stderr, b"", "{case}");
    };

    // No watcher: the inbox's turn is blocked with the watch to start.
    let inbox = [
        ("DAGQ_BIN", binary),
        ("DAGQ_ROLE", "inbox"),
        ("DAGQ_QUEUE", db),
    ];
    let output = stop(false, &inbox);
    assert!(output.status.success());
    assert_eq!(output.stderr, b"");
    let control: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(control["decision"], "block", "{control}");
    let cursor = stdout_json(&launcher(
        &with_db,
        &data_home,
        dir.path(),
        &["status", "--role", "inbox"],
    ))["cursor"]
        .as_i64()
        .unwrap();
    let reason = control["reason"].as_str().unwrap();
    assert!(
        reason.contains(&format!(
            "\" watch --role inbox --until-attention --after {cursor}."
        )),
        "{reason}"
    );
    assert!(reason.contains("run_in_background"), "{reason}");
    assert!(reason.contains("no shell loop"), "{reason}");

    // Once blocked, the next stop goes through; so do the other sessions,
    // and a missing or broken binary or queue.
    silent(&stop(true, &inbox), "stop_hook_active");
    silent(
        &stop(false, &[("DAGQ_BIN", binary), ("DAGQ_QUEUE", db)]),
        "no role",
    );
    for role in ["worker", "planner", "observer"] {
        silent(
            &stop(
                false,
                &[
                    ("DAGQ_BIN", binary),
                    ("DAGQ_ROLE", role),
                    ("DAGQ_QUEUE", db),
                ],
            ),
            role,
        );
    }
    silent(
        &stop(false, &[("DAGQ_ROLE", "inbox"), ("DAGQ_QUEUE", db)]),
        "no dagq",
    );
    let bogus = dir.path().join("not-executable");
    fs::write(&bogus, "").unwrap();
    silent(
        &stop(
            false,
            &[
                ("DAGQ_BIN", bogus.to_str().unwrap()),
                ("DAGQ_ROLE", "inbox"),
                ("DAGQ_QUEUE", db),
            ],
        ),
        "a binary that cannot run",
    );
    let missing = dir.path().join("missing/queue.db");
    silent(
        &stop(
            false,
            &[
                ("DAGQ_BIN", binary),
                ("DAGQ_ROLE", "inbox"),
                ("DAGQ_QUEUE", missing.to_str().unwrap()),
            ],
        ),
        "a status that fails",
    );

    // A watch running: nothing to say.
    let mut watch = Command::new(binary);
    watch.without_actor_env();
    let watch = watch
        .args([
            "--db",
            db,
            "watch",
            "--role",
            "inbox",
            "--until-attention",
            "--interval",
            "1",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    // It never returns by itself: killed by the guard however the test ends.
    let _watch = common::KillOnDrop::new(watch, "the watch until attention");
    {
        let _waiting = common::within(common::STEP_LIMIT, "the watch to be watching");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while stdout_json(&launcher(
            &with_db,
            &data_home,
            dir.path(),
            &["status", "--role", "inbox"],
        ))["inbox_watcher"]["watching"]
            != 1
        {
            assert!(std::time::Instant::now() < deadline, "no watcher");
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
    silent(&stop(false, &inbox), "a watch running");
}

/// `XDG_DATA_HOME` is always pointed away from the developer's real queues.
fn launcher(env: &[(&str, &str)], data_home: &Path, cwd: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(plugin_root().join("bin/dagq"));
    // As the user, not as the actor of the session running the tests.
    command.without_actor_env();
    command
        .env_remove("DAGQ_BIN")
        .env_remove("DAGQ_DB")
        .env("XDG_DATA_HOME", data_home)
        .env("PATH", "/usr/bin:/bin")
        .current_dir(cwd)
        .args(args);
    for (key, value) in env {
        command.env(key, value);
    }
    command.bounded_output().unwrap()
}

fn stdout_json(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn launcher_resolves_the_binary_and_the_repository_queue_under_the_data_home() {
    let dir = tempfile::tempdir().unwrap();
    let data_home = dir.path().join("xdg");
    let repo = dir.path().join("repo");
    fs::create_dir(&repo).unwrap();
    assert!(
        Command::new(git_executable().expect("git executable"))
            .args(["init", "-q", "-b", "main"])
            .current_dir(&repo)
            .bounded_status()
            .unwrap()
            .success()
    );
    let binary = env!("CARGO_BIN_EXE_dagq");
    let env = [("DAGQ_BIN", binary)];
    let git_dir = repo.join(".git").canonicalize().unwrap();
    let hash = dagq::infrastructure::location::repository_hash(&git_dir);
    let expected_db = data_home.join("dagq").join(&hash).join("queue.db");

    let resolved = stdout_json(&launcher(&env, &data_home, &repo, &["--resolve"]));
    assert_eq!(resolved["binary"], binary);
    assert_eq!(resolved["binary_version"], dagq::VERSION);
    assert_eq!(resolved["plugin_version"], plugin_manifest()["version"]);
    assert_eq!(resolved["db"], expected_db.to_str().unwrap());
    assert_eq!(resolved["db_exists"], false);
    assert_eq!(resolved["source"], "repository");
    assert_eq!(resolved["git_common_dir"], git_dir.to_str().unwrap());
    assert_eq!(
        resolved["runs_dir"],
        expected_db.with_file_name("runs").to_str().unwrap()
    );
    assert_eq!(
        resolved["repo"],
        repo.canonicalize().unwrap().to_str().unwrap()
    );

    // `init` creates the missing directory; other commands do not.
    let listing = launcher(&env, &data_home, &repo, &["list"]);
    assert!(!listing.status.success());
    assert!(!data_home.exists());
    let init = stdout_json(&launcher(&env, &data_home, &repo, &["init"]));
    assert_eq!(init["db"], expected_db.to_str().unwrap());
    let added = stdout_json(&launcher(
        &env,
        &data_home,
        &repo,
        &[
            "add",
            "plugin smoke",
            "--acceptance",
            "shown",
            "--verify",
            "true",
        ],
    ));
    assert_eq!(added["status"], "draft");
    let id = added["id"].to_string();
    stdout_json(&launcher(
        &env,
        &data_home,
        &repo,
        &["ready", &id, "--bypass-review"],
    ));
    let shown = stdout_json(&launcher(&env, &data_home, &repo, &["show", &id]));
    assert_eq!(shown["task"]["status"], "ready");
    assert_eq!(shown["task"]["verification_commands"][0], "true");
    assert_eq!(
        stdout_json(&launcher(&env, &data_home, &repo, &["--resolve"]))["db_exists"],
        true
    );
    // A worktree of the same repository shares the queue.
    let worktree = dir.path().join("wt");
    assert!(
        Command::new(git_executable().expect("git executable"))
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(["commit", "-q", "--allow-empty", "-m", "init"])
            .current_dir(&repo)
            .bounded_status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new(git_executable().expect("git executable"))
            .args(["worktree", "add", "-q", "--detach"])
            .arg(&worktree)
            .current_dir(&repo)
            .bounded_status()
            .unwrap()
            .success()
    );
    assert_eq!(
        stdout_json(&launcher(&env, &data_home, &worktree, &["candidates"]))[0]["id"],
        added["id"]
    );
    // An explicit database path wins over the convention, for --resolve too.
    let other = dir.path().join("other.db");
    let env_db = [("DAGQ_BIN", binary), ("DAGQ_DB", other.to_str().unwrap())];
    let init = stdout_json(&launcher(&env_db, &data_home, &repo, &["init"]));
    assert_eq!(init["db"], other.to_str().unwrap());
    let resolved = stdout_json(&launcher(&env_db, &data_home, dir.path(), &["--resolve"]));
    assert_eq!(resolved["db"], other.to_str().unwrap());
    assert_eq!(resolved["source"], "db_flag");
    assert_eq!(resolved["repo"], "");
    // Pass-through flags need no database.
    let version = launcher(&env, &data_home, dir.path(), &["--version"]);
    assert!(version.status.success());
    assert!(String::from_utf8_lossy(&version.stdout).starts_with("dagq "));
}

#[test]
fn launcher_reports_missing_binary_and_repository_as_json_errors() {
    let dir = tempfile::tempdir().unwrap();
    let data_home = dir.path().join("xdg");
    let missing = launcher(&[], &data_home, dir.path(), &["--resolve"]);
    assert!(!missing.status.success());
    let error: Value = serde_json::from_slice(&missing.stderr).unwrap();
    let message = error["error"].as_str().unwrap();
    assert!(message.contains("cargo install --locked dagq"), "{message}");
    assert!(message.contains("~/.cargo/bin"), "{message}");
    assert!(message.contains("DAGQ_BIN"), "{message}");
    // Only the published install path: no Release assets and no advice that
    // applies only when developing dagq itself.
    for absent in [
        "github.com/hisamekms/dagq/releases",
        "aarch64-apple-darwin",
        ".tar.gz",
        "SHA256SUMS",
        "cargo build",
        "dagq repository",
    ] {
        assert!(!message.contains(absent), "{absent}: {message}");
    }

    let bogus = dir.path().join("not-executable");
    fs::write(&bogus, "").unwrap();
    let bad = launcher(
        &[("DAGQ_BIN", bogus.to_str().unwrap())],
        &data_home,
        dir.path(),
        &["list"],
    );
    assert!(!bad.status.success());
    let error: Value = serde_json::from_slice(&bad.stderr).unwrap();
    assert!(
        error["error"]
            .as_str()
            .unwrap()
            .contains("not an executable")
    );

    // Outside a repository the binary itself explains how to point at a queue.
    let outside = launcher(
        &[("DAGQ_BIN", env!("CARGO_BIN_EXE_dagq"))],
        &data_home,
        dir.path(),
        &["list"],
    );
    assert!(!outside.status.success());
    let error: Value = serde_json::from_slice(&outside.stderr).unwrap();
    let message = error["error"].as_str().unwrap();
    assert!(message.contains("--db"), "{message}");
    assert!(message.contains("not inside a Git repository"), "{message}");
    assert!(!data_home.exists());
}

#[test]
fn the_marketplace_offers_this_repository_s_plugin_from_its_own_path() {
    let marketplace: Value = serde_json::from_str(
        &fs::read_to_string(repository_root().join(".claude-plugin/marketplace.json")).unwrap(),
    )
    .unwrap();
    // `claude plugin marketplace add hisamekms/dagq` reads this file, and
    // `claude plugin install claude-dagq@dagq` the entry below.
    assert_eq!(marketplace["name"], "dagq");
    assert_eq!(marketplace["owner"]["name"], "hisamekms");
    let plugins = marketplace["plugins"].as_array().expect("plugins");
    assert_eq!(plugins.len(), 1);
    let entry = &plugins[0];
    assert_eq!(entry["name"], plugin_manifest()["name"]);
    // Two forms (ADR-t617-1): the repository-relative path before the first
    // pinned release, or this repository's plugin directory at a release tag.
    let directory = match &entry["source"] {
        Value::String(source) => {
            assert_eq!(source, "./plugins/claude-dagq");
            source.trim_start_matches("./").to_owned()
        }
        Value::Object(source) => {
            assert_eq!(source["source"], "git-subdir", "{entry}");
            assert_eq!(source["url"], "hisamekms/dagq", "{entry}");
            let reference = source["ref"].as_str().expect("a ref");
            let version = reference.strip_prefix('v').expect("a v<X.Y.Z> ref");
            assert_eq!(version.split('.').count(), 3, "{entry}");
            assert!(
                version
                    .split('.')
                    .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit())),
                "{entry}"
            );
            assert!(entry.get("version").is_none(), "{entry}");
            source["path"].as_str().expect("a path").to_owned()
        }
        other => panic!("unexpected source {other}"),
    };
    assert_eq!(repository_root().join(&directory), plugin_root());
    assert!(
        repository_root()
            .join(&directory)
            .join(".claude-plugin/plugin.json")
            .is_file()
    );
}

/// Runs a copy of `scripts/check-plugin-version.sh` in a repository of only
/// the three files it reads, with crate and plugin version `version`.
fn check_plugin_version(version: &str, marketplace: &Value, args: &[&str]) -> Output {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for sub in [
        "scripts",
        ".claude-plugin",
        "plugins/claude-dagq/.claude-plugin",
    ] {
        fs::create_dir_all(root.join(sub)).unwrap();
    }
    fs::copy(
        repository_root().join("scripts/check-plugin-version.sh"),
        root.join("scripts/check-plugin-version.sh"),
    )
    .unwrap();
    fs::write(
        root.join("Cargo.toml"),
        format!("[package]\nname = \"dagq\"\nversion = \"{version}\"\n"),
    )
    .unwrap();
    fs::write(
        root.join("plugins/claude-dagq/.claude-plugin/plugin.json"),
        serde_json::to_string_pretty(
            &serde_json::json!({"name": "claude-dagq", "version": version}),
        )
        .unwrap(),
    )
    .unwrap();
    fs::write(
        root.join(".claude-plugin/marketplace.json"),
        serde_json::to_string_pretty(
            &serde_json::json!({"name": "dagq", "plugins": [marketplace]}),
        )
        .unwrap(),
    )
    .unwrap();
    Command::new("sh")
        .arg(root.join("scripts/check-plugin-version.sh"))
        .args(args)
        .env_remove("GITHUB_REF_TYPE")
        .env_remove("GITHUB_REF_NAME")
        .bounded_output()
        .unwrap()
}

fn pinned_entry(reference: &str) -> Value {
    serde_json::json!({
        "name": "claude-dagq",
        "source": {
            "source": "git-subdir",
            "url": "hisamekms/dagq",
            "path": "plugins/claude-dagq",
            "ref": reference,
        },
    })
}

#[test]
fn check_plugin_version_accepts_the_relative_source_on_dev_and_the_tag_s_own_ref_on_release() {
    let relative = serde_json::json!({"name": "claude-dagq", "source": "./plugins/claude-dagq"});
    let passes = |version: &str, entry: &Value, args: &[&str]| {
        let output = check_plugin_version(version, entry, args);
        assert!(
            output.status.success(),
            "{version} {entry} {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    passes("0.4.0-dev", &relative, &[]);
    passes("0.5.0-dev", &pinned_entry("v0.4.0"), &[]);
    passes("0.4.0", &pinned_entry("v0.4.0"), &[]);
    passes("0.4.0", &pinned_entry("v0.4.0"), &["--tag", "v0.4.0"]);
}

#[test]
fn check_plugin_version_names_what_is_wrong_with_the_marketplace_entry_of_a_tag() {
    let fails = |entry: &Value, expected: &[&str]| {
        let output = check_plugin_version("0.4.0", entry, &["--tag", "v0.4.0"]);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{entry}: {stderr}");
        for text in expected {
            assert!(stderr.contains(text), "{entry}: {stderr} lacks {text}");
        }
    };
    fails(
        &serde_json::json!({"name": "claude-dagq", "source": "./plugins/claude-dagq"}),
        &["relative source", "tag v0.4.0 needs a git-subdir source"],
    );
    fails(
        &pinned_entry("v0.3.0"),
        &["source.ref \"v0.3.0\" but tag v0.4.0 needs ref v0.4.0"],
    );
    let mut versioned = pinned_entry("v0.4.0");
    versioned["version"] = "0.4.0".into();
    fails(&versioned, &["has version \"0.4.0\""]);
    let mut elsewhere = pinned_entry("v0.4.0");
    elsewhere["source"]["path"] = "plugins/other".into();
    elsewhere["source"]["url"] = "someone/else".into();
    fails(
        &elsewhere,
        &[
            "source.path \"plugins/other\" but needs \"plugins/claude-dagq\"",
            "source.url \"someone/else\" but needs \"hisamekms/dagq\"",
        ],
    );
    fails(&pinned_entry("main"), &["needs a release tag v<X.Y.Z>"]);
    // A release commit without a tag still needs the ref of its own version.
    let output = check_plugin_version("0.4.0", &pinned_entry("v0.3.0"), &[]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("release 0.4.0 needs ref v0.4.0"));
}

/// A stub that answers only `--version` and `locate`, which is all `--resolve`
/// asks of the binary. It lets the version comparison be tested without
/// building a second dagq.
fn fake_binary(dir: &Path, name: &str, version: &str) -> PathBuf {
    let path = dir.join(name);
    crate::common::template::script(
        &path,
        format!(
            "#!/bin/sh\ncase \"$1\" in\n\
             --version) echo 'dagq {version}' ;;\n\
             locate) printf '{{\\n  \"db\": \"/fake/queue.db\"\\n}}\\n' ;;\n\
             esac\n"
        ),
    );

    path
}

#[test]
fn launcher_warns_only_when_plugin_and_binary_differ_in_major_minor() {
    let dir = tempfile::tempdir().unwrap();
    let data_home = dir.path().join("xdg");
    let plugin_version = plugin_manifest()["version"].as_str().unwrap().to_string();
    let mut parts = plugin_version.split('.');
    let major: u64 = parts.next().unwrap().parse().unwrap();
    let minor: u64 = parts.next().unwrap().parse().unwrap();

    // Same major.minor, different patch: a resolution with nothing on stderr.
    let same = format!("{major}.{minor}.99");
    let binary = fake_binary(dir.path(), "same", &same);
    let output = launcher(
        &[("DAGQ_BIN", binary.to_str().unwrap())],
        &data_home,
        dir.path(),
        &["--resolve"],
    );
    let resolved = stdout_json(&output);
    assert_eq!(resolved["plugin_version"], plugin_version);
    assert_eq!(resolved["binary_version"], same);
    assert_eq!(resolved["db"], "/fake/queue.db");
    assert_eq!(
        output.stderr,
        b"",
        "matching versions must not warn: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // One minor apart: the same resolution on stdout plus a warning, exit 0.
    let other = format!("{major}.{}.0", minor + 1);
    let binary = fake_binary(dir.path(), "other", &other);
    let output = launcher(
        &[("DAGQ_BIN", binary.to_str().unwrap())],
        &data_home,
        dir.path(),
        &["--resolve"],
    );
    let resolved = stdout_json(&output);
    assert_eq!(resolved["binary_version"], other);
    let warning: Value = serde_json::from_slice(&output.stderr).unwrap();
    let message = warning["warning"].as_str().expect("warning");
    assert!(message.contains(&plugin_version), "{message}");
    assert!(message.contains(&other), "{message}");
    assert!(
        message.contains("claude plugin update claude-dagq@dagq"),
        "{message}"
    );
    assert!(message.contains("cargo install --locked dagq"), "{message}");
    assert!(!message.contains("releases"), "{message}");
}
