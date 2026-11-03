//! Runtime tests: what a review reads of the landing branch's commit, the
//! Git boundary of the run's revise limit (ADR-t2599-1, ADR-t3033-1). Each
//! attempt reads the code paths and the revise limit from main's commit as
//! it starts, never from the run's branch or the main checkout's
//! uncommitted files, and a commit whose settings do not parse fails the
//! review. The count across resumes, the judgment past the limit and the
//! revise to the live session are the wiring tests'
//! (`compose::wiring_tests::review_revise_limit`); the decisions are the
//! unit tests' (`domain::review_reason`, `domain::run::history`,
//! `supervise::landing`).
use crate::runtime_review::revising_agent;
use crate::runtime_support;

use runtime_support::*;

/// A `revise` verdict script whose reasons carry codes: each `(text,
/// codes)`.
fn coded_revise(reasons: &[(&str, &[&str])]) -> String {
    let reasons: Vec<(&str, &[&str], &[&str])> = reasons
        .iter()
        .map(|(text, codes)| (*text, *codes, &[][..]))
        .collect();
    located_revise(&reasons)
}

/// A `revise` verdict script whose reasons carry codes and paths: each
/// `(text, codes, paths)`, an item without paths printed without them.
fn located_revise(reasons: &[(&str, &[&str], &[&str])]) -> String {
    let reasons: Vec<Value> = reasons
        .iter()
        .map(|(text, codes, paths)| {
            let mut reason = json!({"text": text, "codes": codes});
            if !paths.is_empty() {
                reason["paths"] = json!(paths);
            }
            reason
        })
        .collect();
    let json = json!({"verdict": "revise", "reasons": reasons, "summary": "not yet"});
    format!("printf '%s\\n' '{json}'")
}

/// The one open ask of the queue, an `approve_landing`.
fn open_landing_ask(queue: &mut SqliteQueue) -> dagq::domain::Ask {
    let open: Vec<dagq::domain::Ask> = queue
        .asks(Default::default())
        .unwrap()
        .into_iter()
        .filter(|ask| ask.closed_at.is_none())
        .collect();
    assert_eq!(open.len(), 1, "{open:?}");
    let ask = open[0].clone();
    assert_eq!(ask.kind, dagq::domain::AskKind::ApproveLanding);
    assert_eq!(ask.options, ["land", "send_back", "cancel"]);
    ask
}

/// Commit `config` as main's `dagq.toml`; main's new head.
fn commit_config_on_main(repo: &Path, config: &str) -> String {
    fs::write(repo.join("dagq.toml"), config).unwrap();
    git(repo, &["add", "dagq.toml"]);
    git(repo, &["commit", "-q", "-m", "code paths"]);
    git_out(repo, &["rev-parse", "main"])
}

/// The code paths each `review_started` recorded, in order.
fn started_code_paths(detail: &dagq::domain::TaskDetail) -> Vec<Value> {
    payloads(detail, "review_started")
        .iter()
        .map(|p| p["code_paths"].clone())
        .collect()
}

/// (5) With the landing branch's code paths, past the run's revises an
/// `acceptance_unmet` whose paths all lie outside them is no code finding:
/// with only slips and stale documents beside it the run lands without an
/// ask and each reason becomes a follow_up draft. The run's own branch
/// rewrites `dagq.toml` to make `docs/**` code, which no review reads:
/// every attempt reads main's commit when it starts.
#[test]
fn an_acceptance_unmet_off_the_code_paths_lands_whatever_the_runs_branch_says() {
    let (_dir, repo, db) = fixture();
    let base = commit_config_on_main(&repo, "[review.code]\npaths = [\"src/**\", \"tests/**\"]\n");
    let worker = "if [ \"$TURN\" -eq 1 ]; then \
                    printf '[review.code]\\npaths = [\"docs/**\"]\\n' > dagq.toml; \
                    git add dagq.toml; commit work; \
                  elif [ \"$TURN\" -le 3 ]; then n=$((TURN - 1)); \
                    printf 'fix %s\\n' \"$n\" >> change.txt; git commit -q -am \"fix $n\"; \
                  fi; receipt \"$(git rev-parse HEAD)\"";
    let backend = TestWorkspace::new(&db, false, worker);
    let reviewer = TestReviewer::new(&[
        coded_revise(&[("the loop never ends", &["code_defect"])]),
        coded_revise(&[("add a test", &["test_gap"])]),
        located_revise(&[
            (
                "the design is not written",
                &["acceptance_unmet"],
                &["docs/design/review.md"],
            ),
            ("a typo", &["local_slip"], &[]),
            ("a stale line", &["docs_drift"], &["./docs/a.md"]),
        ]),
    ]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_landed(&repo, &detail.runs[0], "test task", &base);
    assert!(queue.asks(Default::default()).unwrap().is_empty());
    let code = json!(["src/**", "tests/**"]);
    assert_eq!(
        started_code_paths(&detail),
        [code.clone(), code.clone(), code.clone()]
    );
    let finished = payloads(&detail, "review_finished");
    assert!(finished[0].get("reason_paths").is_none(), "{finished:?}");
    assert_eq!(
        finished[2]["reason_paths"],
        json!([["docs/design/review.md"], [], ["docs/a.md"]])
    );
    assert_eq!(
        payloads(&detail, "revise_limit_judged"),
        [
            &json!({"attempt": 3, "revises": 2, "revise_limit": 2, "converged": true, "code_paths": code,
                 "code_findings": []})
        ]
    );
    let categories: Vec<String> = payloads(&detail, "follow_up_registered")
        .iter()
        .filter(|p| p["from"] == "review")
        .map(|p| {
            let draft = TaskId::new(p["task_id"].as_i64().unwrap());
            let (_, material) = queue.draft_origin(draft).unwrap().unwrap();
            material["category"].as_str().unwrap().to_owned()
        })
        .collect();
    assert_eq!(categories, ["remaining_scope", "improvement", "docs_drift"]);
}

/// (5) An `acceptance_unmet` with a path on the landing branch's code
/// paths is a code finding past the run's revises: a person decides in
/// `approve_landing`, told it is left. The main checkout's uncommitted
/// `dagq.toml`, whose code paths would have let it land, is not read.
#[test]
fn an_acceptance_unmet_on_the_code_paths_asks_whatever_the_main_checkouts_files_say() {
    let (_dir, repo, db) = fixture();
    commit_config_on_main(&repo, "[review.code]\npaths = [\"src/**\"]\n");
    fs::write(
        repo.join("dagq.toml"),
        "[review.code]\npaths = [\"lib/**\"]\n",
    )
    .unwrap();
    let backend = TestWorkspace::new(&db, false, &revising_agent(2));
    let reviewer = TestReviewer::new(&[
        coded_revise(&[("the loop never ends", &["code_defect"])]),
        coded_revise(&[("add a test", &["test_gap"])]),
        located_revise(&[
            (
                "the flag is not wired",
                &["acceptance_unmet"],
                &["docs/a.md", "src/flag.rs"],
            ),
            ("a typo", &["local_slip"], &[]),
        ]),
    ]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::AwaitingIntegration);
    assert_eq!(started_code_paths(&detail)[2], json!(["src/**"]));
    assert_eq!(
        payloads(&detail, "revise_limit_judged"),
        [
            &json!({"attempt": 3, "revises": 2, "revise_limit": 2, "converged": false, "code_paths": ["src/**"],
                 "code_findings": [{"text": "the flag is not wired",
                                    "codes": ["acceptance_unmet"],
                                    "paths": ["docs/a.md", "src/flag.rs"],
                                    "security": false}]})
        ]
    );
    assert!(payloads(&detail, "follow_up_registered").is_empty());
    let ask = open_landing_ask(&mut queue);
    assert!(
        ask.question
            .contains("and code findings are left (acceptance_unmet)"),
        "{}",
        ask.question
    );
}

/// (5) Code paths the landing branch's commit writes but that do not
/// parse leave the attempt unpassed: the review fails to a person with
/// why, no job runs, and the run does not land. The main checkout's files
/// parse, so only the commit is at fault.
#[test]
fn code_paths_of_the_landing_branch_that_do_not_parse_fail_the_review() {
    let (_dir, repo, db) = fixture();
    commit_config_on_main(&repo, "[review.code]\npaths = \"src/**\"\n");
    fs::write(
        repo.join("dagq.toml"),
        "[review.code]\npaths = [\"src/**\"]\n",
    )
    .unwrap();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "would pass")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert!(reviewer.prompts().is_empty());
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::AwaitingIntegration);
    assert!(payloads(&detail, "review_finished").is_empty());
    let failed = payloads(&detail, "review_failed");
    assert_eq!(failed.len(), 1);
    let error = failed[0]["error"].as_str().unwrap();
    assert!(
        error.contains("parse dagq.toml in the landing branch's commit")
            && error.contains("dagq.toml:2: value of paths"),
        "{error}"
    );
}

/// The revise limit each `review_started` recorded, in order.
fn started_limits(detail: &dagq::domain::TaskDetail) -> Vec<Value> {
    payloads(detail, "review_started")
        .iter()
        .map(|p| p["revise_limit"].clone())
        .collect()
}

/// (ADR-t3033-1) The revise limit is read by each review attempt from the
/// landing branch's commit as it starts, not once when the supervisor
/// starts, nor from the main checkout's files. Main's commit says 3, the
/// main checkout's uncommitted file 5. The first attempt reads 3 (`revise 1 of 3`). During the
/// second attempt main's commit lowers it to 1: that attempt still sends
/// `revise 2 of 3`, and the third reads 1 and, with two revises sent, is
/// judged past the limit at once, asking a person and naming both.
#[test]
fn the_revise_limit_is_read_from_the_landing_branchs_commit_at_each_attempt() {
    let (_dir, repo, db) = fixture();
    commit_config_on_main(&repo, "[review]\nrevise_limit = 3\n");
    let backend = TestWorkspace::new(&db, false, &revising_agent(2));
    // Run in the run's worktree, the second review commits on main in the
    // main checkout before its verdict, which leaves the checkout clean;
    // then the checkout's file is rewritten without a commit.
    let lower = format!(
        "git -C \"{repo}\" checkout -q main -- . && printf '[review]\\nrevise_limit = 1\\n' > \"{repo}/dagq.toml\" && git -C \"{repo}\" commit -q -am 'lower the revise limit' && printf '[review]\\nrevise_limit = 5\\n' > \"{repo}/dagq.toml\"; {}",
        verdict("revise", &["still short"], "not yet"),
        repo = repo.display()
    );
    fs::write(repo.join("dagq.toml"), "[review]\nrevise_limit = 5\n").unwrap();
    let reviewer = TestReviewer::new(&[
        verdict("revise", &["still short"], "not yet"),
        lower,
        coded_revise(&[("the loop never ends", &["code_defect"])]),
    ]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = detail.runs[0].clone();
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    assert_eq!(
        git_out(&repo, &["show", "main:dagq.toml"]),
        "[review]\nrevise_limit = 1"
    );
    assert_eq!(started_limits(&detail), [json!(3), json!(3), json!(1)]);
    assert_eq!(payloads(&detail, "revise_requested").len(), 2);
    let texts = session_texts(&run);
    for says in ["(revise 1 of 3 of the run)", "(revise 2 of 3 of the run)"] {
        assert!(
            texts.iter().any(|text| text.contains(says)),
            "{says}: {texts:?}"
        );
    }
    assert_eq!(
        payloads(&detail, "revise_limit_judged"),
        [
            &json!({"attempt": 3, "revises": 2, "revise_limit": 1, "converged": false, "code_paths": null,
                 "code_findings": [{"text": "the loop never ends", "codes": ["code_defect"],
                                    "paths": [], "security": false}]})
        ]
    );
    let ask = open_landing_ask(&mut queue);
    assert!(
        ask.question.contains(
            "after the run's 2 revises (its limit is 1), and code findings are left (code_defect)"
        ),
        "{}",
        ask.question
    );
}
