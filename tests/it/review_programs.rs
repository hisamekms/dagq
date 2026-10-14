//! Runtime tests: the program reviews at the head of a run's review
//! (ADR-t1895-2). The programs the landing branch's `dagq.toml` names whose
//! paths the reviewed change touches run one at a time in the configured
//! order before any agent; the first that exits non-zero sends the run back
//! to its worker with its output, and neither the programs after it nor an
//! agent's job start. Once every one passes, the agents review as before. A
//! program none of whose paths the change touches never runs, and a review
//! that needs none records nothing of them. Which program a step runs, and
//! that a program that cannot start or finish fails the review instead of
//! sending the worker back, is `domain::review_programs::tests`.
//!
//! The built-in program builtin:secrets (ADR-t2259-1) runs in a repository
//! whose `dagq.toml` names none; the tests of the project's programs turn
//! it off so that only theirs run.
use crate::runtime_support;

use runtime_support::*;

/// Turns builtin:secrets off, ahead of the project's programs.
const SECRETS_OFF: &str = "[review.programs.\"builtin:secrets\"]\nenabled = false\n";

/// Commit `config` after [`SECRETS_OFF`] as `dagq.toml` and each
/// `(path, text)` script on main.
fn commit_on_main(repo: &Path, config: &str, scripts: &[(&str, String)]) {
    fs::write(repo.join("dagq.toml"), format!("{SECRETS_OFF}{config}")).unwrap();
    git(repo, &["add", "dagq.toml"]);
    fs::create_dir_all(repo.join("scripts")).unwrap();
    for (path, text) in scripts {
        fs::write(repo.join(path), text).unwrap();
        git(repo, &["add", path]);
    }
    git(repo, &["commit", "-q", "-m", "review programs"]);
}

/// A script that appends a line to `mark` each time it runs.
fn marking(mark: &Path) -> String {
    format!(
        "#!/bin/sh\necho ran >> {}\n",
        crate::common::shell_path(mark)
    )
}

/// How many times the script marking `mark` ran.
fn runs_of(mark: &Path) -> usize {
    fs::read_to_string(mark).map_or(0, |text| text.lines().count())
}

/// `(program, outcome)` of each `review_program_finished`, in order.
fn program_outcomes(detail: &dagq::domain::TaskDetail) -> Vec<(String, String)> {
    payloads(detail, "review_program_finished")
        .iter()
        .map(|p| {
            (
                p["program"].as_str().unwrap().to_owned(),
                p["outcome"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

/// The worker's first commit lacks `fix 1`, which `fix` checks: `first`
/// passes, `fix` rejects it, `later` does not run and no agent's job
/// starts; the worker is sent back with `fix`'s name and the end of its
/// output, as a revise of the round. Its fixed commit passes all three,
/// and the one agent's job reviews it and it lands. `never`, whose paths
/// the change does not touch, never runs. The program reviews' events are
/// in the run's events and its timeline.
#[test]
fn a_rejecting_program_sends_the_worker_back_before_any_agent_and_a_passing_set_goes_on() {
    let (fixture, repo, db) = fixture();
    let marks = fixture.dir.path().join("marks");
    fs::create_dir(&marks).unwrap();
    let mark = |name: &str| marks.join(name);
    commit_on_main(
        &repo,
        "[review.programs.first]\nscript = \"scripts/first.sh\"\npaths = [\"change.txt\"]\n\
         [review.programs.fix]\nscript = \"scripts/fix.sh\"\npaths = [\"change.txt\"]\n\
         [review.programs.later]\nscript = \"scripts/later.sh\"\npaths = [\"change.txt\"]\n\
         [review.programs.never]\nscript = \"scripts/never.sh\"\npaths = [\"nothing/**\"]\n",
        &[
            ("scripts/first.sh", marking(&mark("first"))),
            (
                "scripts/fix.sh",
                "#!/bin/sh\ngrep -q 'fix 1' change.txt && exit 0\necho 'change.txt lacks fix 1'\nexit 1\n"
                    .to_owned(),
            ),
            ("scripts/later.sh", marking(&mark("later"))),
            ("scripts/never.sh", marking(&mark("never"))),
        ],
    );
    let backend = TestWorkspace::new(&db, false, &crate::runtime_review::revising_agent(1));
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    let run = detail.runs[0].clone();
    assert_eq!(run.status(), RunStatus::Integrated);
    // One agent's job: after the fix, never before it.
    assert_eq!(reviewer.prompts().len(), 1);
    assert_eq!(payloads(&detail, "review_started").len(), 1);
    assert_eq!(
        program_outcomes(&detail),
        [
            ("first", "passed"),
            ("fix", "rejected"),
            ("first", "passed"),
            ("fix", "passed"),
            ("later", "passed"),
        ]
        .map(|(p, o)| (p.to_owned(), o.to_owned()))
    );
    assert_eq!(
        (
            runs_of(&mark("first")),
            runs_of(&mark("later")),
            runs_of(&mark("never"))
        ),
        (2, 1, 0)
    );
    // The selected programs, in order, without `never`.
    let started = payloads(&detail, "review_programs_started");
    assert_eq!(started.len(), 2);
    let names: Vec<&Value> = started[0]["programs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| &p["name"])
        .collect();
    assert_eq!(names, [&json!("first"), &json!("fix"), &json!("later")]);
    assert_eq!(started[0]["attempt"], 1);
    assert_eq!(started[0]["retried"], false);
    // Each run of them is a round of its own, which numbers its output.
    assert_eq!(
        (&started[0]["round"], &started[1]["round"]),
        (&json!(1), &json!(2))
    );
    let run_dir = Path::new(run.run_dir().unwrap());
    let rejected = fs::read_to_string(run_dir.join("review-program-1-fix.out")).unwrap();
    assert_eq!(rejected, "change.txt lacks fix 1\n");
    assert!(run_dir.join("review-program-2-fix.out").is_file());
    let finished = payloads(&detail, "review_programs_finished");
    assert_eq!(finished.len(), 2);
    assert_eq!(
        (&finished[0]["outcome"], &finished[0]["program"]),
        (&json!("rejected"), &json!("fix"))
    );
    assert_eq!(
        (&finished[1]["outcome"], &finished[1]["program"]),
        (&json!("passed"), &Value::Null)
    );
    // The revise names the program and the end of its output, and counts.
    let requested = payloads(&detail, "revise_requested");
    assert_eq!(requested.len(), 1);
    let reason = requested[0]["reasons"][0].as_str().unwrap();
    for part in [
        "the review program fix rejected the change",
        "change.txt lacks fix 1",
    ] {
        assert!(reason.contains(part), "{part:?} in {reason}");
    }
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "review_programs_finished") < position(&kinds, "revise_requested"));
    assert!(position(&kinds, "revise_finished") < position(&kinds, "review_started"));
    // The timeline reads them with their payloads.
    let timeline = crate::common::cli::ok(&db, &["timeline", run.id().as_str(), "--full"]);
    let shown: Vec<&Value> = timeline["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| {
            e["kind"]
                .as_str()
                .is_some_and(|k| k.starts_with("review_program"))
        })
        .collect();
    assert_eq!(shown.len(), 2 + 5 + 2, "{timeline}");
    assert_eq!(shown[0]["payload"]["programs"][1]["name"], "fix");
}

/// A program none of whose paths the change touches does not run, and a
/// review that needs no program reviews as before, with no event of them.
#[test]
fn a_review_that_needs_no_program_reviews_as_before() {
    let (fixture, repo, db) = fixture();
    let mark = fixture.dir.path().join("never");
    commit_on_main(
        &repo,
        "[review.programs.never]\nscript = \"scripts/never.sh\"\npaths = [\"nothing/**\"]\n",
        &[("scripts/never.sh", marking(&mark))],
    );
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::Integrated);
    assert_eq!(reviewer.prompts().len(), 1);
    assert_eq!(runs_of(&mark), 0);
    assert!(
        !event_kinds(&detail)
            .iter()
            .any(|k| k.starts_with("review_program")),
        "{:?}",
        event_kinds(&detail)
    );
}

/// A worker whose first commit adds what looks like an AWS access key to
/// `change.txt`'s line 2 and whose second takes it out.
const LEAKING_AGENT: &str = "if [ \"$TURN\" -eq 1 ]; then commit work; \
     printf 'key = %s%s\\n' AKIA Q7ZK2M4N6P8R0T3V >> change.txt; git commit -q -am leak; \
     else printf 'clean\\n' > change.txt; git commit -q -am fix; fi; \
     receipt \"$(git rev-parse HEAD)\"";

/// In a repository with no `dagq.toml`, builtin:secrets runs at the head
/// of the review: the worker's commit that adds a key-like line goes back
/// to the worker before any agent's job, with the file, the line and the
/// kind and without the value, as a revise; its fixed commit passes, the
/// agent reviews it and it lands. `review_programs_started` records the
/// built-in's name, the dagq build and the digest of the script it ran
/// (ADR-t2259-1 decisions 1, 5 and 9).
#[test]
fn builtin_secrets_sends_an_added_key_back_without_any_configuration() {
    let (_fixture, repo, db) = fixture();
    assert!(!repo.join("dagq.toml").exists());
    let backend = TestWorkspace::new(&db, false, LEAKING_AGENT);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::Integrated);
    assert_eq!(reviewer.prompts().len(), 1, "no agent before the fix");
    assert_eq!(
        program_outcomes(&detail),
        [
            ("builtin:secrets", "rejected"),
            ("builtin:secrets", "passed")
        ]
        .map(|(p, o)| (p.to_owned(), o.to_owned()))
    );
    let requested = payloads(&detail, "revise_requested");
    assert_eq!(requested.len(), 1);
    let reason = requested[0]["reasons"][0].as_str().unwrap();
    assert!(
        reason.contains("the review program builtin:secrets rejected the change")
            && reason.contains("change.txt:2: aws_access_key_id"),
        "{reason}"
    );
    assert!(!reason.contains("Q7ZK2M4N6P8R0T3V"), "{reason}");
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "revise_requested") < position(&kinds, "review_started"));
    let started = payloads(&detail, "review_programs_started");
    assert_eq!(started.len(), 2);
    let program = &started[0]["programs"][0];
    assert_eq!(program["name"], "builtin:secrets");
    assert_eq!(program["builtin"]["builtin"], "builtin:secrets");
    assert_eq!(program["builtin"]["build"], dagq::VERSION);
    let digest = program["builtin"]["digest"].as_str().unwrap();
    assert_eq!(digest.len(), 64, "{program}");
    assert!(digest.chars().all(|c| c.is_ascii_hexdigit()), "{program}");
    assert_eq!(started[1]["programs"][0]["builtin"], program["builtin"]);
}

/// Run `dagq review-program builtin:secrets` with `args` in `repo`, as its
/// program job does: its exit code and stdout and stderr.
fn secrets_in(repo: &Path, args: &[&str]) -> (Option<i32>, String) {
    use crate::common::Bounded;
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_dagq"))
        .current_dir(repo)
        .args(["review-program", "builtin:secrets"])
        .args(args)
        .bounded_output()
        .unwrap();
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned()
            + &String::from_utf8_lossy(&output.stderr),
    )
}

/// Commit everything in `repo` and return the commit.
fn commit_all(repo: &Path, message: &str) -> String {
    git(repo, &["add", "-A"]);
    git(
        repo,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            message,
        ],
    );
    git_out(repo, &["rev-parse", "HEAD"])
}

/// The process of builtin:secrets reads Git's diff of the range whatever
/// the repository's configuration and attributes say: a file moved in from
/// a path it is not given stays a rename (its lines were already there)
/// with renames off in the configuration, names are right without
/// prefixes, a file the attributes call binary is read as text, a name Git
/// quotes matches the path it is given, and a range it cannot read is no
/// pass. The value is never printed (ADR-t2259-1 decision 1; the judgement
/// of each line is `domain::review_programs::secrets::tests`).
#[test]
fn builtin_secrets_reads_gits_diff_whatever_the_repository_configures() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    git(repo, &["init", "-q"]);
    let dashes = "-".repeat(5);
    let key = format!("{dashes}BEGIN EC PRIVATE KEY{dashes}");
    let aws = format!("id = {}{}", "AKIA", "Q7ZK2M4N6P8R0T3V");
    fs::create_dir_all(repo.join("old")).unwrap();
    fs::write(repo.join("old/f.txt"), format!("first\n{aws}\n{key}\n")).unwrap();
    fs::write(repo.join("kept.txt"), format!("{aws}\n")).unwrap();
    let base = commit_all(repo, "base");
    for (key, value) in [
        ("diff.renames", "false"),
        ("diff.noprefix", "true"),
        ("diff.mnemonicPrefix", "true"),
        ("diff.relative", "true"),
        ("core.quotePath", "true"),
    ] {
        git(repo, &["config", key, value]);
    }
    fs::create_dir_all(repo.join("src")).unwrap();
    git(repo, &["mv", "old/f.txt", "src/f.txt"]);
    fs::write(repo.join(".gitattributes"), "*.env -diff\n").unwrap();
    fs::write(repo.join("src/x.env"), format!("ok\n{aws}\n")).unwrap();
    let quoted = "src/a\tb\\c.txt";
    fs::write(repo.join(quoted), format!("{key}\n")).unwrap();
    let head = commit_all(repo, "head");

    let (code, output) = secrets_in(repo, &[&base, &head, "src/f.txt", quoted]);
    assert_eq!(code, Some(1), "{output}");
    assert!(
        output.starts_with(&format!("{quoted}:1: private_key\nbuiltin:secrets: 1 ")),
        "{output}"
    );
    let (code, output) = secrets_in(repo, &[&base, &head]);
    assert_eq!(code, Some(1), "{output}");
    for found in [
        format!("{quoted}:1: private_key\n"),
        "src/x.env:2: aws_access_key_id\n".to_owned(),
    ] {
        assert!(output.contains(&found), "{found:?} in {output}");
    }
    for absent in ["src/f.txt", "kept.txt", "Q7ZK2M4N6P8R0T3V", "BEGIN"] {
        assert!(!output.contains(absent), "{absent} in {output}");
    }
    // Removing lines adds none.
    fs::remove_file(repo.join("src/x.env")).unwrap();
    let removed = commit_all(repo, "remove");
    assert_eq!(
        secrets_in(repo, &[&head, &removed]),
        (Some(0), String::new())
    );
    // A range it cannot read, or an option for a commit, is no pass.
    for args in [["nope", head.as_str()], ["--output=x", head.as_str()]] {
        let (code, output) = secrets_in(repo, &args);
        assert_eq!(code, Some(2), "{args:?}: {output}");
    }
    assert!(!repo.join("x").exists());
}
