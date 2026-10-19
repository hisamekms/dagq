//! Runtime tests: the records of a worker's local checks (ADR-t2589-1
//! decisions 2 to 5). Each turn of a headless worker is told the records'
//! directory under its run directory, the verification `integrate` runs is
//! not, and the review's material lists the records whose tree is the
//! receipt's commit's with how many were left out, never a log's text,
//! and which of the local gates the landing branch declares the diff
//! requires (decision 6).
use crate::runtime_support;

use runtime_support::headless::*;
use runtime_support::*;

/// The turn: it logs the directory it was told and whether it was there,
/// commits, and writes a record on its commit's tree, one on the tree
/// before, one whose tree changed while it ran, a malformed one and a link
/// to the first, with a log. (A later turn is told the directory as the
/// first is: `actor_executor`'s unit test of a new and a resumed session.)
const TURNS: &str = r#"
printf '%s %s\n' "${DAGQ_LOCAL_CHECKS_DIR-unset}" "$([ -d "$DAGQ_LOCAL_CHECKS_DIR" ] && echo made)" >> "$RUN_DIR/checks-env.log"
rec() {
  printf '{"command":"%s","exit_status":%s,"started_at":"2026-10-17T00:00:00Z","duration_ms":12,"kind":"%s","tree_start":"%s","tree_end":"%s","log":"%s.log"}' "$2" "$3" "$4" "$5" "$6" "$1" > "$DAGQ_LOCAL_CHECKS_DIR/$1.json"
}
commit work
NOW=$(git rev-parse 'HEAD^{tree}'); BEFORE=$(git rev-parse 'HEAD~1^{tree}')
printf 'the log text stays out\n' > "$DAGQ_LOCAL_CHECKS_DIR/a.log"
rec a 'run the checks' 101 gate "$NOW" "$NOW"
rec b 'run on the old tree' 0 gate "$BEFORE" "$BEFORE"
rec c 'run while the tree changed' 0 gate "$BEFORE" "$NOW"
printf 'not a record' > "$DAGQ_LOCAL_CHECKS_DIR/d.json"
ln -s a.json "$DAGQ_LOCAL_CHECKS_DIR/e.json"
receipt "$(git rev-parse HEAD)"; say finished
"#;

#[test]
fn each_turn_is_told_the_records_and_the_review_lists_those_on_the_receipts_tree() {
    let (dir, repo, db, backend) = headless_fixture_verified(
        &[],
        "test -f seed.txt && echo \"checks=${DAGQ_LOCAL_CHECKS_DIR-unset}\"",
    );
    set_turns(dir.path(), TURNS);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    let detail = detail(&db);
    let run = &detail.runs[0];
    let run_dir = Path::new(run.run_dir().unwrap());
    let records = run_dir.join("local-checks");

    // The turn was told the directory, made before it started (decision
    // 2).
    assert_eq!(
        fs::read_to_string(run_dir.join("checks-env.log")).unwrap(),
        format!("{} made\n", records.display())
    );
    // The verification `integrate` ran was not.
    let logs: Vec<String> = fs::read_dir(run_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            let name = path.file_name().unwrap().to_string_lossy();
            name.starts_with("integrate-") && name.contains("-verify-")
        })
        .map(|path| fs::read_to_string(path).unwrap())
        .collect();
    assert!(!logs.is_empty());
    for log in &logs {
        assert!(log.contains("checks=unset"), "{log}");
    }

    // The review's material: only the record on the receipt's tree is
    // listed; the others are counted (decisions 3 and 5).
    let material = fs::read_to_string(run_dir.join("review.md")).unwrap();
    let receipt: Value =
        serde_json::from_str(&fs::read_to_string(run_dir.join("receipt.json")).unwrap()).unwrap();
    let commit = receipt["commit"].as_str().unwrap();
    let tree = git_out(&repo, &["rev-parse", &format!("{commit}^{{tree}}")]);
    let section = material
        .split("\n## Local check records\n")
        .nth(1)
        .unwrap_or_else(|| panic!("no section: {material}"));
    let section = section.split("\n## Commits\n").next().unwrap();
    assert!(
        section.contains(&format!("the tree {tree} of the receipt's commit")),
        "{section}"
    );
    assert!(section.contains("- matched: 1\n"), "{section}");
    assert!(section.contains("while it ran: 2\n"), "{section}");
    assert!(section.contains("over the limits: 2\n"), "{section}");
    let listed: Value = serde_json::from_str(
        section
            .split("```json\n")
            .nth(1)
            .and_then(|rest| rest.split("\n```").next())
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        listed,
        json!([{
            "command": "run the checks",
            "exit_status": 101,
            "started_at": "2026-10-17T00:00:00Z",
            "duration_ms": 12,
            "kind": "gate",
            "log": "a.log",
        }])
    );
    assert!(!material.contains("the log text stays out"));
    // The material comes before the commits, as before.
    assert!(material.find("## Local check records") < material.find("## Commits"));
}

#[test]
fn a_run_whose_scripts_record_nothing_has_no_section() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    set_turns(dir.path(), FINISH);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    let run = &detail(&db).runs[0];
    let run_dir = Path::new(run.run_dir().unwrap());
    assert!(run_dir.join("local-checks").is_dir());
    let material = fs::read_to_string(run_dir.join("review.md")).unwrap();
    assert!(!material.contains("Local check records"), "{material}");
}

/// The records are read from the run directory's files without following
/// a link and up to their limits (decision 4): a record past the limit of
/// bytes or of records is broken, a file that is not `*.json` is no record,
/// and a directory that is a link is one broken record, its target unread.
#[test]
fn the_records_are_read_without_links_and_within_their_limits() {
    use dagq::application::review::read_local_checks;
    use dagq::domain::local_checks::{LocalChecks, MAX_RECORDS, RECORD_BYTES};
    use dagq::infrastructure::run_files::LocalRunFiles;
    const TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
    let dir = tempfile::tempdir().unwrap();
    let run_dir = dir.path().join("runs").join("r1");
    let records = run_dir.join("local-checks");
    fs::create_dir_all(&records).unwrap();
    let record = |tree: &str| {
        json!({
            "command": "check", "exit_status": 0, "started_at": "t", "duration_ms": 1,
            "tree_start": tree, "tree_end": tree, "log": "a.log",
        })
        .to_string()
    };
    // No record yet: nothing to show.
    assert_eq!(
        read_local_checks(&LocalRunFiles, &run_dir, TREE),
        LocalChecks::default()
    );
    fs::write(records.join("a.json"), record(TREE)).unwrap();
    fs::write(records.join("a.log"), "not a record").unwrap();
    fs::write(records.join(".json"), "not a record either").unwrap();
    let mut large = record(TREE);
    large.insert_str(
        1,
        &format!("\"pad\":\"{}\",", "x".repeat(RECORD_BYTES as usize)),
    );
    fs::write(records.join("b.json"), large).unwrap();
    fs::create_dir(records.join("c.json")).unwrap();
    let read = read_local_checks(&LocalRunFiles, &run_dir, TREE);
    assert_eq!(
        (read.matched.len(), read.other_tree, read.broken),
        (1, 0, 2)
    );

    // Past the limit of records, those after it by name are broken.
    for index in 0..MAX_RECORDS {
        fs::write(records.join(format!("z{index:04}.json")), record(TREE)).unwrap();
    }
    let read = read_local_checks(&LocalRunFiles, &run_dir, TREE);
    assert_eq!(read.matched.len() + read.broken, MAX_RECORDS + 3);
    assert_eq!(read.matched.len(), MAX_RECORDS - 2);

    // A directory that is a link is not followed.
    let elsewhere = dir.path().join("elsewhere");
    fs::rename(&records, &elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &records).unwrap();
    let read = read_local_checks(&LocalRunFiles, &run_dir, TREE);
    assert_eq!(
        (read.matched.len(), read.other_tree, read.broken),
        (0, 0, 1)
    );
}

/// The handoff of a new session (ADR-t2852-1) reads the run's records as
/// a review does (ADR-t2589-1 decision 4): only those on the head's tree
/// are kept, the rest of another tree, whose tree changed or that are
/// broken counted, a directory that is a link read as one broken record
/// with its target unread. Past the limit of records it reads the newest,
/// so a check on the final tree recorded last is kept and the older ones
/// counted, while the review's reading stays as it was.
#[test]
fn a_handoff_keeps_the_newest_records_on_the_heads_tree_as_a_review_does() {
    use dagq::application::review::{newest_local_checks, read_local_checks};
    use dagq::domain::local_checks::MAX_RECORDS;
    use dagq::infrastructure::run_files::LocalRunFiles;
    const TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
    const OLD: &str = "1111111111111111111111111111111111111111";
    let dir = tempfile::tempdir().unwrap();
    let run_dir = dir.path().join("runs").join("r1");
    let records = run_dir.join("local-checks");
    fs::create_dir_all(&records).unwrap();
    let record = |start: &str, end: &str, command: &str| {
        json!({
            "command": command, "exit_status": 0, "started_at": "t", "duration_ms": 1,
            "tree_start": start, "tree_end": end, "log": "a.log",
        })
        .to_string()
    };
    // The same exclusions as a review's material.
    fs::write(records.join("a.json"), record(TREE, TREE, "on the head")).unwrap();
    fs::write(records.join("b.json"), record(OLD, OLD, "of another tree")).unwrap();
    fs::write(records.join("c.json"), record(OLD, TREE, "changed")).unwrap();
    fs::write(records.join("d.json"), "not a record").unwrap();
    let (read, older) = newest_local_checks(&LocalRunFiles, &run_dir, TREE);
    assert_eq!(read, read_local_checks(&LocalRunFiles, &run_dir, TREE));
    assert_eq!(older, 0);
    let commands: Vec<&str> = read.matched.iter().map(|c| c.command.as_str()).collect();
    assert_eq!(commands, ["on the head"]);
    assert_eq!((read.other_tree, read.broken), (2, 1));

    // Past the limit, the newest: the last by name, on the final tree.
    for index in 0..MAX_RECORDS {
        fs::write(
            records.join(format!("e{index:04}.json")),
            record(OLD, OLD, "old"),
        )
        .unwrap();
    }
    fs::write(
        records.join("f.json"),
        record(TREE, TREE, "on the final tree"),
    )
    .unwrap();
    let (read, older) = newest_local_checks(&LocalRunFiles, &run_dir, TREE);
    assert_eq!(older, 5);
    let commands: Vec<&str> = read.matched.iter().map(|c| c.command.as_str()).collect();
    assert_eq!(commands, ["on the final tree"]);
    assert_eq!((read.other_tree, read.broken), (MAX_RECORDS - 1, 0));
    let review = read_local_checks(&LocalRunFiles, &run_dir, TREE);
    let commands: Vec<&str> = review.matched.iter().map(|c| c.command.as_str()).collect();
    assert_eq!(commands, ["on the head"]);

    // A directory that is a link is not followed.
    let elsewhere = dir.path().join("elsewhere");
    fs::rename(&records, &elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &records).unwrap();
    let (read, older) = newest_local_checks(&LocalRunFiles, &run_dir, TREE);
    assert_eq!(
        (read.matched.len(), read.other_tree, read.broken, older),
        (0, 0, 1, 0)
    );
}

/// The local gates the landing branch's `dagq.toml` declares (decision 6):
/// the review's section says which the diff from the merge base with the
/// landing branch to the receipt's commit requires, by the changed lines
/// that are in a test as the declared Rust profile tells them apart, and
/// by any change of a test's path while no profile is declared. The
/// section joins the records' in the material before its commits, as the
/// first test shows of the records.
#[test]
fn the_review_says_which_declared_gates_the_diff_requires() {
    use dagq::application::review::local_gates_section;
    use dagq::infrastructure::{adapters::GitRepository, run_env::gate_config};
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    fs::create_dir(&repo).unwrap();
    crate::common::template::repository(&repo, "fixture\n");
    fs::create_dir(repo.join("src")).unwrap();
    let write = |path: &str, text: &str| fs::write(repo.join(path), text).unwrap();
    write(
        "src/a.rs",
        "fn a() {}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() {}\n}\n",
    );
    write("src/b.rs", "fn b() {}\n\n#[test]\nfn t() {}\n");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "base"]);
    let base = git_out(&repo, &["rev-parse", "HEAD"]);
    // The run: a line of `src/a.rs`'s test module and only the production
    // code of `src/b.rs`; no file under `tests/`.
    git(&repo, &["checkout", "-q", "-b", "run"]);
    write(
        "src/a.rs",
        "fn a() {}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() { assert!(true); }\n}\n",
    );
    write("src/b.rs", "fn b() { let _ = 2; }\n\n#[test]\nfn t() {}\n");
    git(&repo, &["commit", "-q", "-am", "work"]);
    let head = git_out(&repo, &["rev-parse", "HEAD"]);
    // The landing branch declares the gates after the run began.
    git(&repo, &["checkout", "-q", "main"]);
    const GATES: &str = "[local_checks]\ntests = [\"src/**\", \"tests/**\"]\n\n\
         [local_checks.gates.stress]\npaths = [\"src/**\", \"tests/**\"]\nkind = \"stress\"\n\n\
         [local_checks.gates.slow]\npaths = [\"tests/**\"]\nkind = \"time-gate\"\n";
    write(
        "dagq.toml",
        &format!("[toolchains.rust]\nprofile = \"rust\"\n\n{GATES}"),
    );
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "declare the gates"]);
    let repository = GitRepository::inspect(&repo).unwrap();

    let section = local_gates_section(&repository, &gate_config, &base, &head)
        .unwrap_or_else(|| panic!("no section"));
    assert!(
        section.starts_with("\n## Local gates\n\nWhether the diff "),
        "{section}"
    );
    // Its prose is one paragraph, not a block indented as code.
    let prose = section.split("```").next().unwrap();
    assert!(!prose.contains("  "), "{section}");
    // The diff starts at the merge base with the landing branch.
    assert!(
        section.contains(&format!("the diff {base}..{head}")),
        "{section}"
    );
    let gates: Value = serde_json::from_str(
        section
            .split("```json\n")
            .nth(1)
            .and_then(|rest| rest.split("\n```").next())
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        gates,
        json!([
            {
                "gate": "stress", "kind": "stress", "required": true,
                "reason": "a test in its paths changed", "changed_tests": ["src/a.rs"],
            },
            {
                "gate": "slow", "kind": "time-gate", "required": false,
                "reason": "no test in its paths changed", "changed_tests": [],
            },
        ])
    );

    // Without a declared profile, no line is told apart: the change of
    // `src/b.rs`'s production code counts as a test's too.
    write("dagq.toml", GATES);
    git(&repo, &["commit", "-q", "-am", "no profile"]);
    let section = local_gates_section(&repository, &gate_config, &base, &head).unwrap();
    assert!(
        section.contains("\"changed_tests\": [\n      \"src/a.rs\",\n      \"src/b.rs\"\n    ]"),
        "{section}"
    );

    // A landing branch that declares the tests and no gate judges none.
    write("dagq.toml", "[local_checks]\ntests = [\"**\"]\n");
    git(&repo, &["commit", "-q", "-am", "no gate"]);
    assert_eq!(
        local_gates_section(&repository, &gate_config, &base, &head),
        None
    );
}
