//! End-to-end happy path in a repository that is not dagq's own (goal 52):
//! its default branch is `master`, it has no `origin`, and it has neither
//! a `Cargo.toml` nor an `AGENTS.md`.
use super::*;

/// A stub worker's task lands on `master` of a repository unlike dagq's:
/// the landing branch is guessed as `master` (ADR-t615-1), the push is
/// skipped because there is no `origin`, and the migration the run adds
/// under a number `master` already has is landed as it is, since the
/// renumbering is for dagq's source repository only (ADR-t614-1).
#[test]
#[ignore = "an e2e; run with --ignored"]
fn a_task_lands_on_master_of_a_repository_without_origin_cargo_toml_or_agents_md() {
    let fixture = fixture_on(
        "master",
        &[("migrations/0001_x.sql", "CREATE TABLE x (id INTEGER);\n")],
    );
    let Fixture {
        repo, base, env, ..
    } = &fixture;
    for absent in ["Cargo.toml", "AGENTS.md", "CLAUDE.md"] {
        assert!(!repo.join(absent).exists(), "{absent}");
    }
    assert_eq!(git(repo, &["remote"]), "");
    assert_eq!(git(repo, &["branch", "--list", "main"]), "");
    let doctor = dagq(env, &["doctor"]);
    let settings = &doctor["repository"];
    assert_eq!(settings["branch"], "master", "{doctor}");
    assert_eq!(settings["branch_source"], "master", "{doctor}");
    assert_eq!(settings["remote"], "origin", "{doctor}");
    assert_eq!(settings["remote_exists"], false, "{doctor}");
    assert!(settings["error"].is_null(), "{doctor}");

    let task_id = add_ready_task_described(
        env,
        "e2e task on master",
        "Add e2e.txt and a migration to the worktree. E2E-MIGRATION E2E-REVIEW-PASS",
        &[],
        &[],
    );
    let pass = supervise_once(&fixture, &[], &[&task_id]);
    let outcome = &pass.outcome;
    assert_eq!(outcome["errors"], Value::Array(vec![]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");

    let detail = dagq(env, &["show", &task_id, "--full"]);
    assert_eq!(detail["task"]["status"], "completed");
    let run = &detail["runs"][0];
    let run_id = run["id"].as_str().unwrap();
    assert_eq!(run["base_commit"], base.as_str());
    // One squash commit on master, which the checkout followed; no main
    // was made.
    let master = git(repo, &["rev-parse", "master"]);
    assert_eq!(run["result_commit"], master.as_str());
    assert_eq!(git(repo, &["rev-parse", "master^"]), base.as_str());
    assert_eq!(git(repo, &["branch", "--list", "main"]), "");
    assert_eq!(git(repo, &["branch", "--show-current"]), "master");
    assert_eq!(git(repo, &["status", "--porcelain"]), "");
    assert_eq!(
        fs::read_to_string(repo.join("e2e.txt")).unwrap(),
        format!("written by the stub agent for {run_id}\n")
    );
    // Both migrations keep number 0001: nothing was renumbered.
    assert_eq!(
        git(repo, &["ls-tree", "--name-only", "master", "migrations/"]),
        "migrations/0001_e2e.sql\nmigrations/0001_x.sql"
    );

    let events = detail["events"].as_array().unwrap();
    let kinds: Vec<&str> = events.iter().map(|e| e["kind"].as_str().unwrap()).collect();
    for kind in [
        "migration_renumbered",
        "migration_number_taken",
        "push_finished",
        "push_failed",
    ] {
        assert!(!kinds.contains(&kind), "{kind} in {kinds:?}");
    }
    let skipped = events
        .iter()
        .find(|e| e["kind"] == "push_skipped")
        .unwrap_or_else(|| panic!("no push_skipped in {kinds:?}"));
    assert_eq!(
        skipped["payload"],
        json!({
            "remote": "origin",
            "branch": "master",
            "commit": master,
            "reason": "the repository has no remote origin",
        })
    );
    // The reads that once named refs/heads/main work on master.
    let stats = dagq(env, &["stats"]);
    assert!(stats.is_object(), "{stats}");
    let status = dagq(env, &["status"]);
    assert_eq!(status["runs"], Value::Array(vec![]), "{status}");
    assert_eq!(
        dagq(env, &["integrate", "--next"])["outcome"],
        "no_run_awaiting"
    );
}
