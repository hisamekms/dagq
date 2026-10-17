//! Runtime tests: the records of a worker's local checks (ADR-t2589-1
//! decisions 2 to 5). Each turn of a headless worker is told the records'
//! directory under its run directory, the verification `integrate` runs is
//! not, and the review's material lists the records whose tree is the
//! receipt's commit's with how many were left out, never a log's text.
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
