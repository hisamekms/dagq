//! Runtime tests: the built-in review agents (ADR-t2259-1 decisions 4 to
//! 6 and 9), one representative case through a run's review: a built-in
//! is selected from the landing branch's commit and started as an agent
//! job of its own, with the binary's definition and the project's rules
//! from that commit only, and its start records which built-in of which
//! build judged. Which built-ins `enabled` and `paths` select is decided
//! in `application::review::tests` and `domain::review_builtins::tests`.
use crate::runtime_support;

use runtime_support::*;
use sha2::{Digest, Sha256};

/// A repository without `dagq.toml` whose main has rules for
/// `builtin:security`, and a worker that changes `app.py` (code that the
/// default globs match) and rewrites those rules on its branch: the
/// review starts `builtin:security`'s job beside the whole review's, and
/// not `builtin:tests`'. Its prompt holds the binary's definition and
/// main's rules, not the worker's; its start records the built-in, this
/// build and both digests.
#[test]
fn a_builtin_reviews_with_the_binarys_definition_and_the_landing_branchs_rules() {
    let (_dir, repo, db) = fixture();
    const MAIN_RULES: &str = "Payments go through pay/ only.\n";
    const RULES: &str = ".dagq/review/builtin-security.md";
    fs::create_dir_all(repo.join(".dagq/review")).unwrap();
    fs::write(repo.join(RULES), MAIN_RULES).unwrap();
    git(&repo, &["add", RULES]);
    git(&repo, &["commit", "-q", "-m", "review rules"]);
    let worker = format!(
        "printf 'the worker s rules\\n' > {RULES} && printf 'print(1)\\n' > app.py && \
         git add {RULES} app.py; commit work; receipt \"$(git rev-parse HEAD)\"; idle; await_exit"
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
