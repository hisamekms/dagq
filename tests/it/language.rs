//! The language AI writes in for people (ADR-t616-2): with nothing set the
//! prompts carry no instruction; the user's `config.toml` sets it; the
//! repository's `dagq.toml` is over it. The worker's and the review's
//! prompts carry the instruction, and `doctor` and `status --role inbox`
//! show the language and where it came from.
use crate::{common::Bounded, runtime_support};

use runtime_support::*;

const LANGUAGE: &str = "Language: write everything you address to people";

/// Supervise one pass with the user's `config.toml` at `user_config`, and
/// the worker's and the review's prompts of task 1's run.
fn prompts(db: &Path, repo: &Path, user_config: &Path) -> (String, String) {
    let backend = TestWorkspace::new(db, false, VALID_AGENT);
    let outcome = supervise_with(
        db,
        repo,
        &backend,
        &SuperviseOptions {
            user_config: Some(user_config.to_owned()),
            ..supervise_options(4, true)
        },
    )
    .unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let run = SqliteQueue::open(db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap()
        .runs[0]
        .clone();
    let run_dir = Path::new(run.run_dir().unwrap());
    (
        fs::read_to_string(run_dir.join("prompt.txt")).unwrap(),
        fs::read_to_string(run_dir.join("review-prompt-1.txt")).unwrap(),
    )
}

/// `dagq doctor` and `dagq status --role inbox` with `XDG_CONFIG_HOME` at
/// `config_home`: each one's `language`.
fn reported(db: &Path, config_home: &Path) -> (Value, Value) {
    let run = |args: &[&str]| -> Value {
        let output = Command::new(env!("CARGO_BIN_EXE_dagq"))
            .env("XDG_CONFIG_HOME", config_home)
            .env_remove("DAGQ_ROLE")
            .arg("--db")
            .arg(db)
            .args(args)
            .bounded_output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        serde_json::from_slice(&output.stdout).unwrap()
    };
    (
        run(&["doctor"])["language"].clone(),
        run(&["status", "--role", "inbox"])["language"].clone(),
    )
}

#[test]
fn nothing_set_adds_no_instruction() {
    let (dir, repo, db) = fixture();
    let config_home = dir.path().join("config home");
    let (worker, review) = prompts(&db, &repo, &config_home.join("dagq/config.toml"));
    assert!(!worker.contains(LANGUAGE), "{worker}");
    assert!(!review.contains(LANGUAGE), "{review}");
    let (doctor, status) = reported(&db, &config_home);
    assert_eq!(doctor["tag"], Value::Null, "{doctor}");
    assert_eq!(doctor["source"], "unset", "{doctor}");
    assert_eq!(doctor["instruction"], Value::Null, "{doctor}");
    assert_eq!(
        doctor["user_config"],
        config_home.join("dagq/config.toml").to_str().unwrap()
    );
    assert_eq!(status["source"], "unset", "{status}");
}

#[test]
fn the_users_language_is_in_every_prompt() {
    let (dir, repo, db) = fixture();
    let config_home = dir.path().join("config home");
    let user_config = config_home.join("dagq/config.toml");
    fs::create_dir_all(user_config.parent().unwrap()).unwrap();
    fs::write(&user_config, "[language]\ntag = \"ja\"\n").unwrap();
    let (worker, review) = prompts(&db, &repo, &user_config);
    let instruction = dagq::domain::language::instruction("ja");
    assert!(worker.ends_with(&instruction), "{worker}");
    assert!(review.ends_with(&instruction), "{review}");
    let (doctor, status) = reported(&db, &config_home);
    assert_eq!(doctor["tag"], "ja", "{doctor}");
    assert_eq!(doctor["source"], "user", "{doctor}");
    // The SessionStart hook prints the instruction the inbox's status carries.
    assert_eq!(status["instruction"], instruction.as_str(), "{status}");
}

#[test]
fn the_repositorys_language_is_over_the_users() {
    let (dir, repo, db) = fixture();
    let config_home = dir.path().join("config home");
    let user_config = config_home.join("dagq/config.toml");
    fs::create_dir_all(user_config.parent().unwrap()).unwrap();
    fs::write(&user_config, "[language]\ntag = \"ja\"\n").unwrap();
    // The other readers of dagq.toml accept the table as it is.
    fs::write(
        repo.join("dagq.toml"),
        "[language]\ntag = \"pt-BR\"\n[stall]\nidle_without_receipt_secs = 600\n",
    )
    .unwrap();
    git(&repo, &["add", "dagq.toml"]);
    git(&repo, &["commit", "-m", "name the language"]);
    let (worker, review) = prompts(&db, &repo, &user_config);
    let instruction = dagq::domain::language::instruction("pt-BR");
    assert!(worker.ends_with(&instruction), "{worker}");
    assert!(review.ends_with(&instruction), "{review}");
    assert!(!worker.contains("`ja`"), "{worker}");
    let (doctor, status) = reported(&db, &config_home);
    assert_eq!(doctor["tag"], "pt-BR", "{doctor}");
    assert_eq!(doctor["source"], "repository", "{doctor}");
    assert_eq!(status["tag"], "pt-BR", "{status}");

    // A mistake shows in doctor and leaves the prompts without the
    // instruction; it never stops a claim or a landing.
    fs::write(repo.join("dagq.toml"), "[language]\ntag = \"Portuguese\"\n").unwrap();
    git(&repo, &["commit", "-am", "name a wrong language"]);
    let mut queue = SqliteQueue::open(&db).unwrap();
    add_ready_task(&mut queue, "second", &[]);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let outcome = supervise_with(
        &db,
        &repo,
        &backend,
        &SuperviseOptions {
            user_config: Some(user_config.clone()),
            ..supervise_options(4, true)
        },
    )
    .unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let run = queue.show(TaskId::new(2)).unwrap().runs[0].clone();
    let second = fs::read_to_string(Path::new(run.run_dir().unwrap()).join("prompt.txt")).unwrap();
    assert!(!second.contains(LANGUAGE), "{second}");
    let (doctor, _) = reported(&db, &config_home);
    assert_eq!(doctor["source"], "unset", "{doctor}");
    let error = doctor["error"].as_str().unwrap();
    assert!(
        error.contains("dagq.toml:2") && error.contains("BCP 47"),
        "{doctor}"
    );
}
