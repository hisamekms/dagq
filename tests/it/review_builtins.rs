//! Runtime tests: the built-in review agents (ADR-t2259-1 decisions 4 to
//! 6 and 9), one representative case through a run's review: a built-in
//! is selected from the landing branch's commit and started as an agent
//! job of its own, with the binary's definition and the project's rules
//! from that commit only, and its start records which built-in of which
//! build judged. The whole review is `builtin:acceptance`: its prompt
//! holds the landing branch's rules for it or none, never the worktree's
//! instructions, and its start records the same. Which built-ins `enabled` and `paths` select is decided
//! in `application::review::tests` and `domain::review_builtins::tests`.
use crate::runtime_support;

use runtime_support::*;
use sha2::{Digest, Sha256};

/// A repository without `dagq.toml` whose main has rules for
/// `builtin:security` and for the whole review, and a worker that changes
/// `app.py` (code that the default globs match) and rewrites both rules on
/// its branch: the review starts `builtin:security`'s job beside the whole
/// review's, and not `builtin:tests`'. Both prompts hold main's rules, not
/// the worker's, the agent's after the binary's definition; the rule on
/// copies of the code and history reaches the whole review from main's
/// file only, never from the runtime (ADR-t2389-1 decision 2). Both
/// starts record the built-in, this build and the digests.
#[test]
fn a_builtin_reviews_with_the_binarys_definition_and_the_landing_branchs_rules() {
    let (_dir, repo, db) = fixture();
    const MAIN_RULES: &str = "Payments go through pay/ only.\n";
    const RULES: &str = ".dagq/review/builtin-security.md";
    const MAIN_ACCEPTANCE: &str = "Report as docs_drift text a change adds to a document that copies the code or carries history.\n";
    const ACCEPTANCE: &str = ".dagq/review/builtin-acceptance.md";
    fs::create_dir_all(repo.join(".dagq/review")).unwrap();
    fs::write(repo.join(RULES), MAIN_RULES).unwrap();
    fs::write(repo.join(ACCEPTANCE), MAIN_ACCEPTANCE).unwrap();
    git(&repo, &["add", RULES, ACCEPTANCE]);
    git(&repo, &["commit", "-q", "-m", "review rules"]);
    let worker = format!(
        "printf 'the worker s rules\\n' > {rules} && \
         printf 'the worker s acceptance\\n' > {acceptance} && printf 'print(1)\\n' > app.py && \
         git add {rules} {acceptance} app.py; commit work; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
        rules = shell_path(RULES),
        acceptance = shell_path(ACCEPTANCE),
    );
    let backend = TestWorkspace::new(&db, false, &worker);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::Integrated);

    let started = payloads(&detail, "review_job_started");
    let agents: Vec<&Value> = started.iter().map(|s| &s["agent"]).collect();
    assert_eq!(agents, [&json!(null), &json!("builtin:security")]);
    let definition = dagq::domain::review_builtins::BuiltinAgent::Security.definition();
    let digest = format!("{:x}", Sha256::digest(definition.as_bytes()));
    assert_eq!(
        started[1]["builtin"],
        json!({"builtin": "builtin:security", "build": dagq::VERSION, "digest": digest,
               "rules": {"path": RULES,
                         "digest": format!("{:x}", Sha256::digest(MAIN_RULES.as_bytes()))}})
    );
    assert_eq!(
        started[0]["builtin"],
        json!({"builtin": "builtin:acceptance", "build": dagq::VERSION,
               "digest": dagq::application::prompt::acceptance_digest(),
               "rules": {"path": ACCEPTANCE,
                         "digest": format!("{:x}", Sha256::digest(MAIN_ACCEPTANCE.as_bytes()))}})
    );
    let whole = reviewer.prompts();
    assert_eq!(whole.len(), 1, "{whole:?}");
    assert!(whole[0].contains(MAIN_ACCEPTANCE), "{}", whole[0]);
    assert_eq!(
        whole[0].matches("copies the code").count(),
        1,
        "{}",
        whole[0]
    );
    assert!(
        !whole[0].contains("the worker s acceptance"),
        "{}",
        whole[0]
    );
    let launches = reviewer.launches();
    assert_eq!(launches.len(), 1, "{launches:?}");
    let prompt = &launches[0].prompt;
    for part in [
        format!("<definition>\n{definition}\n</definition>"),
        format!("<project-rules>\n{MAIN_RULES}\n</project-rules>"),
    ] {
        assert!(prompt.contains(&part), "{part} in {prompt}");
    }
    assert!(!prompt.contains("the worker s rules"), "{prompt}");
    // The reviewed head has the worker's rules.
    let head = detail.runs[0].result_commit().unwrap().to_string();
    assert_eq!(
        git_out(&repo, &["show", &format!("{head}:{RULES}")]).trim(),
        "the worker s rules"
    );
    assert_eq!(
        payloads(&detail, "review_started")[0]["subagents"]["agents"],
        json!([{"agent": "builtin:security", "paths": ["app.py"],
                "definition": "builtin:security", "digest": digest}])
    );
}

/// A repository whose main has an `AGENTS.md` and no rules for the whole
/// review, and a worker that puts `.dagq/review/builtin-acceptance.md` in
/// its worktree and commits it on its branch: the whole review's prompt
/// has no rules of the project and names no instructions file to read,
/// and its start records `builtin:acceptance` without rules.
#[test]
fn the_whole_review_without_the_landing_branchs_rules_reads_no_instructions() {
    let (_dir, repo, db) = fixture();
    const AGENTS: &str = "Judge every change by the house rules.\n";
    const ACCEPTANCE: &str = ".dagq/review/builtin-acceptance.md";
    fs::write(repo.join("AGENTS.md"), AGENTS).unwrap();
    git(&repo, &["add", "AGENTS.md"]);
    git(&repo, &["commit", "-q", "-m", "instructions"]);
    let worker = format!(
        "mkdir -p .dagq/review && printf 'the worker s acceptance\\n' > {acceptance} && \
         printf 'notes\\n' > notes.txt && \
         git add {acceptance} notes.txt; commit work; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
        acceptance = shell_path(ACCEPTANCE),
    );
    let backend = TestWorkspace::new(&db, false, &worker);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::Integrated);
    let started = payloads(&detail, "review_job_started");
    assert_eq!(started.len(), 1, "{started:?}");
    assert_eq!(
        started[0]["builtin"],
        json!({"builtin": "builtin:acceptance", "build": dagq::VERSION,
               "digest": dagq::application::prompt::acceptance_digest(), "rules": null})
    );
    let whole = reviewer.prompts();
    assert_eq!(whole.len(), 1, "{whole:?}");
    for word in [
        "AGENTS.md",
        "CLAUDE.md",
        AGENTS,
        "the worker s acceptance",
        "The project's rules",
    ] {
        assert!(!whole[0].contains(word), "{word} in {}", whole[0]);
    }
}
