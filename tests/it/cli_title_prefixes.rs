//! The prefixes of a task's title on a real queue: the `[tasks]
//! title_prefixes` of the main checkout's `dagq.toml` holds `lint` and
//! `submit` to them, while `add` and `edit` take any title; without the key
//! no title is checked.
use crate::common::cli::{ok, submit_from};
use crate::runtime_support::*;

#[test]
fn the_title_prefixes_of_dagq_toml_hold_lint_and_submit_but_not_add_or_edit() {
    let (_fixture, repo, db) = fixture();
    let repository = GitRepository::inspect(&repo).unwrap();
    SqliteQueue::open(&db)
        .unwrap()
        .bind_repository(
            &dagq::infrastructure::adapters::path_text(&repository.common_dir).unwrap(),
        )
        .unwrap();
    fs::write(
        repo.join("dagq.toml"),
        "[tasks]\ntitle_prefixes = [\"feat\", \"fix\"]\n",
    )
    .unwrap();
    let sound = ["--acceptance", "works", "--verify", "x"];
    let add = |title: &str| ok(&db, &[&["add", title][..], &sound].concat())["id"].to_string();
    // `add` takes a title outside the prefixes, and `edit` keeps one.
    let good = add("feat(cli): x");
    let bad = add("runtime: x");
    let edited = ok(&db, &["edit", &bad, "--title", "runtime: y"]);
    assert_eq!(edited["title"], "runtime: y");

    let linted = ok(&db, &["lint", &good, &bad]);
    let violations = linted["violations"].as_array().unwrap();
    assert_eq!(violations.len(), 1, "{linted}");
    assert_eq!(violations[0]["code"], "title_prefix_outside_set");
    assert_eq!(violations[0]["task_id"].to_string(), bad);
    let reason = violations[0]["reason"].as_str().unwrap();
    assert!(reason.contains("feat, fix"), "{reason}");
    let submitted = submit_from(&db, None, &[&good, &bad]);
    assert!(!submitted.status.success());
    let error: Value = serde_json::from_slice(&submitted.stderr).unwrap();
    assert_eq!(error["error"], reason, "{error}");
    // Nothing moved: both are still drafts.
    for id in [&good, &bad] {
        assert_eq!(ok(&db, &["show", id])["task"]["status"], "draft");
    }
    ok(&db, &["edit", &bad, "--title", "fix!: y"]);
    assert_eq!(ok(&db, &["lint", &good, &bad])["violations"], json!([]));
    assert!(submit_from(&db, None, &[&good, &bad]).status.success());

    // Without the key, any title is linted and submitted.
    fs::write(repo.join("dagq.toml"), "[tasks]\n").unwrap();
    let free = add("no prefix");
    assert_eq!(ok(&db, &["lint", &free])["violations"], json!([]));
    assert!(submit_from(&db, None, &[&free]).status.success());
}
