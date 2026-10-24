//! The hints of `lint` on a real queue: the `[tasks] related_docs` of the
//! main checkout's `dagq.toml` names the documents `lint` reads from that
//! checkout (those Git does not ignore), an unreadable or deleted one is
//! reported, and `submit` refuses nothing for a hint; without the key
//! there are no hints.
use crate::common::cli::{ok, submit_from};
use crate::runtime_support::*;

#[test]
fn lint_hints_the_related_docs_of_the_main_checkout_and_submit_ignores_them() {
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
        "[tasks]\nrelated_docs = [\"docs/design/**/*.md\"]\n",
    )
    .unwrap();
    fs::create_dir_all(repo.join("docs/design/sub")).unwrap();
    fs::write(
        repo.join("docs/design/flow.md"),
        "# Flow\n## Entry\nThe entry is `load_related_docs`.\n## Other\nnothing\n",
    )
    .unwrap();
    fs::write(
        repo.join("docs/design/sub/named.md"),
        "# Named\n`load_related_docs` again\n",
    )
    .unwrap();
    fs::write(repo.join("docs/design/broken.md"), [0xff, 0xfe]).unwrap();
    // Tracked, then deleted from the checkout: listed, but not readable.
    fs::write(repo.join("docs/design/gone.md"), "# Gone\n").unwrap();
    git(&repo, &["add", "docs/design/gone.md"]);
    fs::remove_file(repo.join("docs/design/gone.md")).unwrap();
    // Outside the glob, or ignored by Git: never read.
    fs::write(repo.join("README.md"), "`load_related_docs`\n").unwrap();
    fs::create_dir_all(repo.join("docs/design/out")).unwrap();
    fs::write(repo.join(".gitignore"), "docs/design/out/\n").unwrap();
    fs::write(
        repo.join("docs/design/out/built.md"),
        "# Built\n`load_related_docs`\n",
    )
    .unwrap();

    let id = ok(
        &db,
        &[
            "add",
            "feat: x",
            "--description",
            "change `load_related_docs`",
            "--context",
            "see docs/design/sub/named.md",
            "--acceptance",
            "works",
            "--verify",
            "x",
        ],
    )["id"]
        .to_string();
    let linted = ok(&db, &["lint", &id]);
    assert_eq!(linted["violations"], json!([]), "{linted}");
    let hints = &linted["hints"];
    assert_eq!(hints.as_array().unwrap().len(), 1, "{linted}");
    assert_eq!(hints[0]["task_id"].to_string(), id);
    assert_eq!(
        hints[0]["sections"],
        json!([{"path": "docs/design/flow.md", "heading": "Entry", "terms": ["load_related_docs"]}])
    );
    assert_eq!(hints[0]["omitted"], 0);
    let unreadable: Vec<&str> = hints[0]["unreadable"]
        .as_array()
        .unwrap()
        .iter()
        .map(|document| document["path"].as_str().unwrap())
        .collect();
    assert_eq!(
        unreadable,
        ["docs/design/broken.md", "docs/design/gone.md"],
        "{linted}"
    );
    assert!(submit_from(&db, None, &[&id]).status.success());

    // Without the key, no hints.
    fs::write(repo.join("dagq.toml"), "[tasks]\n").unwrap();
    let free = ok(
        &db,
        &[
            "add",
            "feat: y",
            "--description",
            "`load_related_docs`",
            "--acceptance",
            "works",
            "--verify",
            "x",
        ],
    )["id"]
        .to_string();
    assert_eq!(ok(&db, &["lint", &free])["hints"], json!([]));
}
