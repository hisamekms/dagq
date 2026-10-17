//! What the toolchain profiles recommend (ADR-t2268-1 decision 7) in
//! `dagq init` and `dagq setup-review`: the profiles the repository
//! declares, or its files suggest, propose their declaration, their
//! verification commands and their review programs; `setup-review` gives
//! its agent only the programs it may write, and writes them only at a
//! person's approval; nothing proposed works until `dagq.toml` has it. A
//! profile made up in `tests/common` and registered beside the built-in
//! one gets there with no other code.

use crate::common::toolchain::PROFILES;
use crate::common::{
    setup_review::{CONFIG, Fixture, git},
    shell_path,
};

use dagq::domain::toolchain::{BUILTIN, rust::RECOMMENDATION};
use dagq::infrastructure::run_env::{load_toolchains, parse_config};
use serde_json::{Value, json};
use std::{fs, os::unix::fs::PermissionsExt, path::Path, time::Duration};

/// Commit `files` (a path and its text) in `repo`.
fn commit(repo: &Path, files: &[(&str, &str)]) {
    for (path, text) in files {
        let path = repo.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    git(repo, &["add", "."]);
    git(repo, &["commit", "-q", "-m", "files"]);
}

/// Neither a profile nor a review program works while `dagq.toml` does not
/// have it, whatever was recommended.
fn nothing_works(repo: &Path) {
    let config = parse_config(&fs::read_to_string(repo.join("dagq.toml")).unwrap()).unwrap();
    assert!(config.toolchains.is_empty());
    assert!(config.review_programs().programs.is_empty());
    assert_eq!(
        load_toolchains(repo, &BUILTIN).unwrap().profiles().count(),
        0
    );
}

/// A fake `claude` that keeps its prompt (its standard input) beside the
/// repository and writes the program `name` from a staged copy: its script
/// at `script` and its `table` appended to `dagq.toml`.
fn writing_agent(fixture: &Fixture, name: &str, script: &str, table: &str, body: &str) -> String {
    let stage = fixture.dir.path().join(format!("stage-{name}"));
    fs::create_dir_all(&stage).unwrap();
    fs::write(stage.join("table"), format!("\n{table}")).unwrap();
    fs::write(stage.join("script"), body).unwrap();
    let path = fixture.dir.path().join(format!("claude-{name}"));
    fs::write(
        &path,
        format!(
            "#!/bin/sh\ncat > {prompt}\nmkdir -p {dir}\ncat {table} >> dagq.toml\n\
             cp {source} {script}\n\
             echo '{{\"type\":\"result\",\"result\":\"done\"}}'\n",
            prompt = shell_path(fixture.dir.path().join(format!("prompt-{name}"))),
            dir = shell_path(Path::new(script).parent().unwrap()),
            table = shell_path(stage.join("table")),
            source = shell_path(stage.join("script")),
            script = shell_path(script),
        ),
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path.to_str().unwrap().to_owned()
}

fn prompt_of(fixture: &Fixture, name: &str) -> String {
    fs::read_to_string(fixture.dir.path().join(format!("prompt-{name}"))).unwrap()
}

#[test]
fn init_and_setup_review_propose_the_rust_profiles_settings_and_write_only_what_is_approved() {
    let fixture = Fixture::new();
    commit(
        &fixture.repo,
        &[
            ("Cargo.toml", "[package]\nname = \"x\"\n"),
            ("src/main.rs", "fn main() {}\n"),
        ],
    );
    // init: the guessed declaration and the recommendations, shown only.
    let init = fixture.ok(&[], &["init"]);
    let toolchains = &init["recommended"]["toolchains"];
    assert_eq!(toolchains["from"], "guessed", "{init}");
    assert_eq!(
        toolchains["toolchains"],
        "[toolchains.rust]\nprofile = \"rust\"\n"
    );
    assert_eq!(
        toolchains["verify"],
        json!([{"profile": "rust", "commands": RECOMMENDATION.verify}])
    );
    let program = RECOMMENDATION.programs[0];
    assert_eq!(toolchains["programs"][0]["name"], program.name);
    assert_eq!(toolchains["programs"][0]["setup_review"], true);
    let config = toolchains["config"].as_str().unwrap();
    assert!(config.contains("[toolchains.rust]") && config.contains(&program.table()));
    assert_eq!(
        fs::read_to_string(fixture.repo.join("dagq.toml")).unwrap(),
        CONFIG
    );
    nothing_works(&fixture.repo);

    // setup-review: the program goes to the agent, the rest is shown.
    let agent = writing_agent(
        &fixture,
        program.name,
        program.script,
        &program.table(),
        program.body,
    );
    let shown = fixture.ok(&[], &["setup-review", "--claude", &agent]);
    let prompt = prompt_of(&fixture, program.name);
    assert!(prompt.contains(&program.table()), "{prompt}");
    assert!(prompt.contains(program.body));
    assert!(!prompt.contains("cargo nextest run"), "{prompt}");
    assert!(!prompt.contains("[toolchains."), "{prompt}");
    assert_eq!(shown["toolchains"]["from"], "guessed", "{shown}");
    assert_eq!(
        shown["toolchains"]["verify"][0]["commands"],
        json!(RECOMMENDATION.verify)
    );
    assert_eq!(shown["approved"], false);
    assert_eq!(git(&fixture.repo, &["status", "--porcelain"]), "");
    nothing_works(&fixture.repo);

    let applied = fixture.ok(&[], &["setup-review", "--yes", "--claude", &agent]);
    assert_eq!(applied["approved"], true, "{applied}");
    let written = fs::read_to_string(fixture.repo.join("dagq.toml")).unwrap();
    assert!(written.contains(&program.table()), "{written}");
    // The declaration is the person's to write: still none.
    assert!(!written.contains("[toolchains."));
    assert_eq!(
        fs::read_to_string(fixture.repo.join(program.script)).unwrap(),
        program.body
    );
    assert!(fixture.places_gone());
}

/// `setup-review` of the queue at `fixture` with the made-up profile
/// registered, the agent `claude`, approved beforehand.
fn setup_with_made_up(fixture: &Fixture, claude: &str) -> anyhow::Result<Value> {
    dagq::compose::setup_review(
        &fixture.db,
        &fixture.repo,
        &dagq::compose::SetupReviewOptions {
            profiles: &PROFILES,
            claude: claude.into(),
            codex: "codex".into(),
            codex_home: None,
            timeout: Duration::from_secs(60),
            approval: dagq::application::setup_review::Approval::Given,
            dagq: env!("CARGO_BIN_EXE_dagq").into(),
        },
    )
}

#[test]
fn a_made_up_profiles_recommendations_reach_init_and_the_generation_within_its_bounds() {
    let fixture = Fixture::new();
    commit(&fixture.repo, &[("tool/made.build", "")]);
    let init = dagq::compose::init_recommended(&fixture.repo, &PROFILES);
    let toolchains = &init["toolchains"];
    assert_eq!(
        toolchains["profiles"],
        json!([{"name": "made-up", "profile": "made-up", "paths": ["tool/**"]}]),
        "{init}"
    );
    assert_eq!(
        toolchains["verify"][0]["commands"],
        json!(["madebuild check"])
    );
    assert_eq!(toolchains["programs"][0]["name"], "made-lint");
    assert_eq!(toolchains["programs"][0]["setup_review"], true);
    assert_eq!(toolchains["programs"][1]["name"], "made-outside");
    assert_eq!(toolchains["programs"][1]["setup_review"], false);
    // The built-in profiles alone know nothing of it.
    let builtin = dagq::compose::init_recommended(&fixture.repo, &BUILTIN);
    assert_eq!(builtin["toolchains"]["from"], Value::Null, "{builtin}");

    // Declared, the made-up profile's program within the bounds is given
    // to the agent and applied.
    commit(
        &fixture.repo,
        &[(
            "dagq.toml",
            &format!("{CONFIG}\n[toolchains.tool]\nprofile = \"made-up\"\npaths = [\"tool/**\"]\n"),
        )],
    );
    let declared = dagq::compose::init_recommended(&fixture.repo, &PROFILES);
    assert_eq!(declared["toolchains"]["from"], "declared", "{declared}");
    assert_eq!(declared["toolchains"]["toolchains"], Value::Null);
    let table = "[review.programs.made-lint]\nscript = \".dagq/review/programs/made-lint.sh\"\npaths = [\"**/*.made\"]\n";
    let lint = writing_agent(
        &fixture,
        "made-lint",
        ".dagq/review/programs/made-lint.sh",
        table,
        "#!/bin/sh\nroot=$(git rev-parse --show-toplevel) || exit 2\nexit 0\n",
    );
    let applied = setup_with_made_up(&fixture, &lint).unwrap();
    assert_eq!(applied["toolchains"]["from"], "declared", "{applied}");
    let prompt = prompt_of(&fixture, "made-lint");
    assert!(prompt.contains(table), "{prompt}");
    assert!(!prompt.contains("made-outside"), "{prompt}");
    assert!(!prompt.contains("madebuild check"), "{prompt}");
    assert_eq!(applied["applied"].as_array().unwrap().len(), 2, "{applied}");
    git(&fixture.repo, &["add", "."]);
    git(&fixture.repo, &["commit", "-q", "-m", "review settings"]);

    // The recommendation outside the bounds, written anyway, is refused
    // and nothing is applied.
    let outside = writing_agent(
        &fixture,
        "made-outside",
        "scripts/made-outside.sh",
        "[review.programs.made-outside]\nscript = \"scripts/made-outside.sh\"\npaths = [\"**\"]\n",
        "#!/bin/sh\nexit 0\n",
    );
    let error = format!("{:#}", setup_with_made_up(&fixture, &outside).unwrap_err());
    assert!(
        error.contains("outside dagq.toml and .dagq/review/, so nothing is applied"),
        "{error}"
    );
    assert!(error.contains("scripts/made-outside.sh"), "{error}");
    assert_eq!(git(&fixture.repo, &["status", "--porcelain"]), "");
    assert!(!fixture.repo.join("scripts").exists());
    assert!(fixture.places_gone());
}
